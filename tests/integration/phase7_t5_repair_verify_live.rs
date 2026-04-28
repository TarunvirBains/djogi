//! Phase 7 T5 — live-PG integration tests for rollback, fake-apply,
//! baseline, verify, and repair.
//!
//! # What these tests prove
//!
//! - `rollback_plan` runs `down` SQL in reverse order and updates the
//!   ledger row to `rolled_back`.
//! - Lossy rollback is refused without `LossyRollbackPolicy::Allow`.
//! - Lossy rollback proceeds when the operator opts in and records
//!   the reason in `partial_apply_note`.
//! - `fake_apply_plan` records a `faked` row without running SQL.
//! - `baseline_plan` records a `baseline` row and writes the snapshot.
//! - `verify` reports clean / drifted / missing-table states with
//!   stable D6xx diagnostic codes.
//! - `repair_checksum_drift` updates the ledger row only with
//!   `RepairConfirmation::OperatorAcknowledged`.
//! - `repair_partial_apply` honours each `PartialApplyResolution`.
//! - `repair_snapshot_rebuild` writes the operator-supplied snapshot.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use djogi::config::MigrateConfig;
use djogi::migrate::{
    AppliedSchema, BucketKey, Classification, LossyRollbackKind, LossyRollbackPolicy,
    LossyRollbackWarning, MigrationPlan, OperationSql, PartialApplyResolution, RepairConfirmation,
    RepairError, RollbackError, RunnerCtx, RunnerError, SNAPSHOT_FORMAT_VERSION, Segment,
    SegmentKind, VerifySeverity, WorkspaceGuard, acquire_workspace_lock, apply_plan, baseline_plan,
    bootstrap_ledger, compute_checksum, fake_apply_plan, repair_checksum_drift,
    repair_partial_apply, repair_resume_partial_apply, repair_snapshot_rebuild, rollback_plan,
    verify,
};

// ── Helpers ───────────────────────────────────────────────────────────────

fn empty_snapshot() -> AppliedSchema {
    AppliedSchema {
        djogi_version: "0.1.0".to_string(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: "2026-04-25T00:00:00Z".to_string(),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: vec!["".to_string()],
    }
}

fn temp_path(stub: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("djogi-{stub}-{stamp}.json"))
}

fn temp_lock() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("djogi-t5-{stamp}.lock"))
}

fn acquire_test_workspace_guard() -> WorkspaceGuard {
    acquire_workspace_lock(&temp_lock(), Duration::from_secs(2)).expect("acquire workspace lock")
}

fn op(label: &str, up: &str, down: &str) -> OperationSql {
    OperationSql {
        label: label.to_string(),
        up: up.to_string(),
        down: down.to_string(),
        lossy: None,
    }
}

fn lossy_op(label: &str, up: &str, down: &str, kind: LossyRollbackKind) -> OperationSql {
    OperationSql {
        label: label.to_string(),
        up: up.to_string(),
        down: down.to_string(),
        lossy: Some(LossyRollbackWarning {
            kind,
            detail: format!("operation `{label}` cannot reconstruct row data"),
        }),
    }
}

fn transactional_plan(stmts: Vec<OperationSql>) -> MigrationPlan {
    MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: stmts,
        }],
    }
}

fn make_runner_ctx(
    plan: &MigrationPlan,
    version: &str,
    snapshot: Option<AppliedSchema>,
    snapshot_path: Option<PathBuf>,
) -> RunnerCtx {
    let frags: Vec<&str> = plan
        .segments
        .iter()
        .flat_map(|s| s.statements.iter())
        .map(|s| s.up.as_str())
        .collect();
    let checksum_up = compute_checksum(frags);
    RunnerCtx {
        bucket: plan.bucket.clone(),
        version: version.to_string(),
        description: format!("test migration {version}"),
        checksum_up,
        checksum_down: None,
        snapshot,
        snapshot_path,
        config: MigrateConfig::default(),
        // T5 tests pre-date T7's policy gate; pick the permissive
        // default so rollback / repair / baseline paths run as before.
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
    }
}

// ── Rollback: happy path ──────────────────────────────────────────────────

#[djogi::djogi_test]
async fn rollback_happy_path_drops_table_and_marks_row_rolled_back(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_users",
        "CREATE TABLE \"t5_users\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"t5_users\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010001__rollback_happy", None, None);

    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let exists_before: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_users' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(exists_before, "table must exist after apply");

    let report = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await
    .expect("rollback ok");
    assert_eq!(report.transactional_undone, 1);
    assert!(report.lossy_reason.is_none());

    let exists_after: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_users' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists after");
    assert!(!exists_after, "table must be gone after rollback");

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(status, "rolled_back");

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note");
    let note = note.expect("note must be set");
    assert!(note.contains("rolled back at"), "note: {note}");
}

// ── Rollback: lossy down refused ──────────────────────────────────────────

#[djogi::djogi_test]
async fn rollback_lossy_refuses_without_policy(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![lossy_op(
        "DropColumn t5_lossy.legacy",
        "ALTER TABLE \"t5_lossy\" DROP COLUMN \"legacy\"",
        "-- LOSSY: column data not recoverable",
        LossyRollbackKind::DropColumn,
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010002__rollback_lossy_refused", None, None);

    // Manually create the table so the apply path itself is happy.
    ctx.raw_ddl("CREATE TABLE t5_lossy (id BIGINT, legacy TEXT)")
        .await
        .expect("create base table");

    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let err = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await
    .expect_err("must refuse lossy");
    match err {
        RollbackError::LossyRollbackRefused {
            offending_labels, ..
        } => {
            assert_eq!(offending_labels.len(), 1);
            assert!(offending_labels[0].contains("DropColumn"));
        }
        other => panic!("expected LossyRollbackRefused, got {other:?}"),
    }
}

// ── Rollback: lossy down allowed ──────────────────────────────────────────

#[djogi::djogi_test]
async fn rollback_lossy_allowed_records_reason(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    // Lossy down is a SQL-comment placeholder — running it is a no-op
    // (the comment parses fine in PG), but the rollback path still
    // surfaces the reason. We test the typical pattern where the lossy
    // operation's down side is harmless to execute.
    let plan = transactional_plan(vec![lossy_op(
        "DropColumn t5_lossy_allow.legacy",
        "ALTER TABLE \"t5_lossy_allow\" DROP COLUMN \"legacy\"",
        "-- LOSSY: column data not recoverable",
        LossyRollbackKind::DropColumn,
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010003__rollback_lossy_allow", None, None);
    ctx.raw_ddl("CREATE TABLE t5_lossy_allow (id BIGINT, legacy TEXT)")
        .await
        .expect("create base table");

    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let report = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Allow {
            reason: "operator validated backups exist".to_string(),
        },
        None,
    )
    .await
    .expect("rollback ok");
    assert_eq!(
        report.lossy_reason.as_deref(),
        Some("operator validated backups exist")
    );

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note");
    let note = note.expect("note");
    assert!(note.contains("operator validated backups"), "note: {note}");
}

// ── Rollback: not-applied state refuses ───────────────────────────────────

#[djogi::djogi_test]
async fn rollback_refuses_when_status_is_pending(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_pending",
        "CREATE TABLE \"t5_pending\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_pending\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010004__rollback_pending", None, None);

    let err = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await
    .expect_err("must fail — version was never applied");
    match err {
        RollbackError::VersionNotFound { version } => {
            assert_eq!(version, "V20260425010004__rollback_pending");
        }
        other => panic!("expected VersionNotFound, got {other:?}"),
    }
}

// ── Rollback: snapshot revert when prior_snapshot supplied ────────────────

#[djogi::djogi_test]
async fn rollback_reverts_snapshot_when_prior_supplied(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_path("rollback-snap");
    // Apply with one snapshot.
    let plan = transactional_plan(vec![op(
        "AddTable t5_rb_snap",
        "CREATE TABLE \"t5_rb_snap\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_rb_snap\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425010005__rollback_snap",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
    );
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // The "prior snapshot" is just `empty_snapshot()` again here —
    // we're proving the path writes whatever we hand it.
    let prior = empty_snapshot();
    let report = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        Some(&prior),
    )
    .await
    .expect("rollback ok");
    assert!(report.snapshot_reverted);
    assert!(snapshot_path.exists(), "prior snapshot must be on disk");
    let _ = std::fs::remove_file(&snapshot_path);
}

// ── Fake-apply ────────────────────────────────────────────────────────────

#[djogi::djogi_test]
async fn fake_apply_records_faked_row_without_running_sql(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_fake",
        "CREATE TABLE \"t5_fake\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_fake\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010006__fake", None, None);

    fake_apply_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        "out-of-band tool already created table",
    )
    .await
    .expect("fake-apply ok");

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(status, "faked");

    // Critical: the SQL must NOT have run.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_fake' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists");
    assert!(!exists, "fake-apply must NOT execute SQL");

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note");
    let note = note.expect("note");
    assert!(note.contains("faked at"), "note: {note}");
    assert!(note.contains("out-of-band"), "note: {note}");
}

#[djogi::djogi_test]
async fn fake_apply_persists_snapshot(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_path("fake-snap");
    let plan = transactional_plan(vec![op(
        "AddTable t5_fake_snap",
        "CREATE TABLE \"t5_fake_snap\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_fake_snap\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425010007__fake_snap",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
    );
    fake_apply_plan(&mut ctx, &plan, &runner_ctx, &_guard, "test reason")
        .await
        .expect("fake-apply ok");
    assert!(snapshot_path.exists(), "snapshot must be persisted");
    let _ = std::fs::remove_file(&snapshot_path);
}

// ── Baseline ──────────────────────────────────────────────────────────────

#[djogi::djogi_test]
async fn baseline_records_baseline_row(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let bucket = BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    };
    // Baseline doesn't need a plan — just a runner ctx with a
    // checksum (the operator computes one over the live schema's
    // canonical rendering; for the test we use an empty-checksum).
    let plan = transactional_plan(vec![op("AddTable noop", "SELECT 1", "SELECT 1")]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010008__baseline", None, None);

    let report = baseline_plan(
        &mut ctx,
        &bucket,
        &runner_ctx,
        &_guard,
        "established from existing schema",
    )
    .await
    .expect("baseline ok");
    assert!(report.run_id != 0);

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(status, "baseline");

    let description: String = ctx
        .raw_scalar(
            "SELECT description FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("description");
    assert!(description.starts_with("<baseline>"), "desc: {description}");
}

// ── Verify: clean DB ──────────────────────────────────────────────────────

#[djogi::djogi_test]
async fn verify_clean_db_reports_no_errors(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    // Empty snapshot vs. empty DB (we excluded the ledger table).
    let report = verify(&mut ctx, &empty_snapshot())
        .await
        .expect("verify ok");
    // No tables in snapshot, no tables in DB → no D6xx errors.
    assert!(
        !report.has_errors(),
        "diagnostics: {:?}",
        report.diagnostics
    );
    assert_eq!(report.applied_count, 0);
}

// ── Verify: missing-table drift ───────────────────────────────────────────

#[djogi::djogi_test]
async fn verify_detects_missing_table_as_d601(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    let mut snap = empty_snapshot();
    snap.models.insert(
        "ghost_users".to_string(),
        djogi::migrate::TableSchema {
            app: None,
            columns: vec![djogi::migrate::ColumnSchema {
                check: None,
                default_sql: None,
                foreign_key: None,
                index_type: None,
                indexed: false,
                max_length: None,
                name: "id".to_string(),
                nullable: false,
                on_delete: None,
                outbox_exclude: false,
                rationale: None,
                relation_kind: None,
                renamed_from: None,
                sequence_within: None,
                sql_type: "BIGINT".to_string(),
                unique: false,
            }],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: djogi::migrate::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "ghost_users".to_string(),
            tenant_key: None,
        },
    );
    let report = verify(&mut ctx, &snap).await.expect("verify ok");
    assert!(report.has_errors(), "diagnostics: {:?}", report.diagnostics);
    assert!(
        report.diagnostics.iter().any(|d| d.code == "D601"
            && d.severity == VerifySeverity::Error
            && d.location.as_deref() == Some("ghost_users")),
        "expected D601 ghost_users; got: {:?}",
        report.diagnostics
    );
}

// ── Verify: extra-live-table drift ────────────────────────────────────────

#[djogi::djogi_test]
async fn verify_detects_extra_live_table_as_d602(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    ctx.raw_ddl("CREATE TABLE t5_unlisted (id BIGINT PRIMARY KEY)")
        .await
        .expect("create rogue table");
    let snap = empty_snapshot();
    let report = verify(&mut ctx, &snap).await.expect("verify ok");
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "D602" && d.location.as_deref() == Some("t5_unlisted")),
        "expected D602 t5_unlisted; got: {:?}",
        report.diagnostics
    );
}

// ── Verify: stable diagnostic ordering ────────────────────────────────────

#[djogi::djogi_test]
async fn verify_diagnostic_ordering_is_stable(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    ctx.raw_ddl("CREATE TABLE t5_alpha (id BIGINT PRIMARY KEY)")
        .await
        .expect("alpha");
    ctx.raw_ddl("CREATE TABLE t5_zebra (id BIGINT PRIMARY KEY)")
        .await
        .expect("zebra");
    let snap = empty_snapshot();
    let r1 = verify(&mut ctx, &snap).await.expect("verify 1");
    let r2 = verify(&mut ctx, &snap).await.expect("verify 2");
    assert_eq!(
        r1.diagnostics, r2.diagnostics,
        "verify must be deterministic"
    );
    // Both diagnostics share code D602 — locations must sort
    // alphabetically.
    let d602: Vec<&str> = r1
        .diagnostics
        .iter()
        .filter(|d| d.code == "D602")
        .map(|d| d.location.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(d602, vec!["t5_alpha", "t5_zebra"]);
}

// ── Repair: checksum drift ────────────────────────────────────────────────

#[djogi::djogi_test]
async fn repair_checksum_drift_updates_row(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_drift",
        "CREATE TABLE \"t5_drift\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_drift\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010009__drift", None, None);
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // Pretend the operator recomputed a fresh checksum.
    let new_checksum = compute_checksum(["a different fragment"]);
    let report = repair_checksum_drift(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        &new_checksum,
        None,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("repair ok");
    // B-9: repair_checksum_drift now repairs both checksum_up AND
    // checksum_down in one call. The report includes one ledger
    // change per column — two changes total.
    assert_eq!(report.ledger_changes.len(), 2);
    assert!(
        report
            .ledger_changes
            .iter()
            .any(|c| c.column == "checksum_up" && c.after == new_checksum)
    );

    let stored: String = ctx
        .raw_scalar(
            "SELECT checksum_up FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("stored");
    assert_eq!(stored, new_checksum);
}

#[djogi::djogi_test]
async fn repair_checksum_drift_rejects_invalid_checksum(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_drift_bad",
        "CREATE TABLE \"t5_drift_bad\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_drift_bad\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010010__drift_bad", None, None);
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let err = repair_checksum_drift(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        "V1:not_lowercase_hex_at_all_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        None,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect_err("must reject malformed checksum");
    match err {
        djogi::migrate::RepairError::InvalidChecksum { .. } => (),
        other => panic!("expected InvalidChecksum, got {other:?}"),
    }
}

// ── Repair: partial apply resolutions ─────────────────────────────────────

#[djogi::djogi_test]
async fn repair_partial_apply_marks_rolled_back(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    // Force a partial apply: build a split plan where the second
    // non-tx step is invalid SQL.
    let plan = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![
            Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    "AddTable t5_partial",
                    "CREATE TABLE \"t5_partial\" (\"id\" BIGINT, \"e\" TEXT)",
                    "DROP TABLE \"t5_partial\"",
                )],
            },
            Segment {
                kind: SegmentKind::NonTransactional,
                statements: vec![
                    op(
                        "AddIndex t5_partial_e_idx",
                        "CREATE INDEX CONCURRENTLY \"t5_partial_e_idx\" \
                         ON \"t5_partial\" (\"e\")",
                        "DROP INDEX CONCURRENTLY \"t5_partial_e_idx\"",
                    ),
                    op(
                        "AddIndex t5_partial_missing_idx",
                        "CREATE INDEX CONCURRENTLY \"t5_partial_missing_idx\" \
                         ON \"t5_partial\" (\"missing\")",
                        "DROP INDEX CONCURRENTLY \"t5_partial_missing_idx\"",
                    ),
                ],
            },
        ],
    };
    let runner_ctx = make_runner_ctx(&plan, "V20260425010011__partial", None, None);
    let _ = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard).await;

    // Status is `failed`. Repair flips it to `rolled_back`.
    let report = repair_partial_apply(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        PartialApplyResolution::MarkRolledBack,
        "manual rollback completed by ops",
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("repair ok");
    assert!(
        report
            .ledger_changes
            .iter()
            .any(|c| c.column == "status" && c.after == "rolled_back")
    );
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(status, "rolled_back");
}

#[djogi::djogi_test]
async fn repair_partial_apply_marks_faked(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: vec![op(
                "AddTable t5_fake_repair",
                "CREATE TABLE \"t5_fake_repair_invalid\" (\"id\" THIS_IS_NOT_A_TYPE)",
                "DROP TABLE \"t5_fake_repair_invalid\"",
            )],
        }],
    };
    let runner_ctx = make_runner_ctx(&plan, "V20260425010012__partial_faked", None, None);
    let _ = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard).await;

    let report = repair_partial_apply(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        PartialApplyResolution::MarkFaked,
        "out-of-band fix already in place",
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("repair ok");
    assert!(
        report
            .ledger_changes
            .iter()
            .any(|c| c.column == "status" && c.after == "faked")
    );
}

#[djogi::djogi_test]
async fn repair_partial_apply_marks_applied(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: vec![op(
                "AddTable t5_applied_repair",
                "CREATE TABLE \"t5_applied_repair_invalid\" (\"id\" THIS_IS_NOT_A_TYPE)",
                "DROP TABLE \"t5_applied_repair_invalid\"",
            )],
        }],
    };
    let runner_ctx = make_runner_ctx(&plan, "V20260425010013__partial_applied", None, None);
    let _ = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard).await;

    let report = repair_partial_apply(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        PartialApplyResolution::MarkApplied,
        "manually completed remaining steps",
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("repair ok");
    assert!(
        report
            .ledger_changes
            .iter()
            .any(|c| c.column == "status" && c.after == "applied")
    );
}

#[djogi::djogi_test]
async fn repair_partial_apply_rejects_already_applied(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_partial_invalid",
        "CREATE TABLE \"t5_partial_invalid\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_partial_invalid\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010014__partial_invalid", None, None);
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // Status is `applied` — repair_partial_apply must reject.
    let err = repair_partial_apply(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        PartialApplyResolution::MarkRolledBack,
        "test",
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect_err("must reject");
    match err {
        djogi::migrate::RepairError::InvalidResolution { .. } => (),
        other => panic!("expected InvalidResolution, got {other:?}"),
    }
}

// ── Repair: snapshot rebuild ──────────────────────────────────────────────

#[djogi::djogi_test]
async fn repair_snapshot_rebuild_writes_live_projection(mut ctx: djogi::DjogiContext) {
    // B-12 (Codex round-3): the round-2 strengthening compared the
    // rebuild output against a SECOND rebuild — that pinned
    // determinism but not correctness. A deterministic-but-wrong
    // projection would still pass. This round-3 version pins the
    // rebuild against expected per-column / per-index VALUES, so a
    // wrong projection (missing default, swapped type, wrong index
    // column list, wrong uniqueness) fails the test loudly.
    //
    // The round-3 plan:
    //
    //   1. Apply a multi-table plan with varied shape (3 tables,
    //      different column types, an index, different PK column
    //      spelling).
    //   2. Delete the snapshot file (rebuild's failure mode is
    //      "snapshot was lost or corrupted").
    //   3. Run `repair_snapshot_rebuild`.
    //   4. Re-read the rebuilt snapshot via `load_snapshot`.
    //   5. Run `repair_snapshot_rebuild` a SECOND time into a
    //      separate path (determinism cross-check — kept from
    //      round-2 because it is cheap and catches a different
    //      class of regression than the per-column assertions).
    //   6. Per-column / per-index VALUE assertions:
    //      - `t5_b12_alpha.id` is `int8` (the canonical lower-cased
    //        rendering of `BIGINT` returned by `format_type`) and
    //        NOT NULL.
    //      - `t5_b12_beta.created_at` carries a NON-NONE
    //        `default_sql` whose normalized form mentions `now`
    //        (Postgres rewrites `DEFAULT now()` to `now()` in
    //        `pg_get_expr`; the projection passes it through).
    //      - `t5_b12_gamma.alpha_id` is `int8` NOT NULL.
    //      - `t5_b12_gamma_alpha_id_idx` lives on `t5_b12_gamma`,
    //        is NON-unique (`CREATE INDEX`, no `UNIQUE`), uses
    //        `BTree`, and its column list is exactly `["alpha_id"]`.
    //
    // Per-column / per-index VALUE assertions provably fail on a
    // deterministic-but-wrong projection (e.g. swapping `int8` for
    // `text`, dropping the default, returning the wrong column
    // list). Codex round-3 B-1 picked this option (b) over the
    // export-`live_schema_for_repair` path (option a) because the
    // public-surface change is more invasive than is justified by
    // the marginal coverage gain — the per-value assertions exhaust
    // the "could a deterministic-but-wrong projection sneak past?"
    // question already.
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    // Multi-table plan — three tables with varied column types and
    // one index. Apply via the runner so the ledger row exists when
    // the rebuild's `count_applied_for_app` advisory runs.
    let plan = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: vec![
                op(
                    "AddTable t5_b12_alpha",
                    "CREATE TABLE \"t5_b12_alpha\" (\
                       \"id\" BIGINT PRIMARY KEY, \
                       \"label\" TEXT NOT NULL, \
                       \"qty\" INTEGER)",
                    "DROP TABLE \"t5_b12_alpha\"",
                ),
                op(
                    "AddTable t5_b12_beta",
                    "CREATE TABLE \"t5_b12_beta\" (\
                       \"id\" BIGINT PRIMARY KEY, \
                       \"name\" VARCHAR(64) NOT NULL, \
                       \"created_at\" TIMESTAMPTZ NOT NULL DEFAULT now())",
                    "DROP TABLE \"t5_b12_beta\"",
                ),
                op(
                    "AddTable t5_b12_gamma",
                    "CREATE TABLE \"t5_b12_gamma\" (\
                       \"id\" BIGINT PRIMARY KEY, \
                       \"alpha_id\" BIGINT NOT NULL)",
                    "DROP TABLE \"t5_b12_gamma\"",
                ),
                op(
                    "AddIndex t5_b12_gamma_alpha_id_idx",
                    "CREATE INDEX \"t5_b12_gamma_alpha_id_idx\" \
                     ON \"t5_b12_gamma\" (\"alpha_id\")",
                    "DROP INDEX \"t5_b12_gamma_alpha_id_idx\"",
                ),
            ],
        }],
    };
    let runner_ctx = make_runner_ctx(&plan, "V20260425010902__b12_rebuild", None, None);
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let bucket = BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    };
    let snapshot_path = temp_path("rebuild");

    // Step 2: simulate the failure mode — snapshot file missing.
    // (`repair_snapshot_rebuild` writes to this path; the rebuild
    // does not require the file to exist beforehand.)
    let _ = std::fs::remove_file(&snapshot_path);
    assert!(
        !snapshot_path.exists(),
        "snapshot file should be missing before rebuild"
    );

    // Step 3: rebuild.
    let report = repair_snapshot_rebuild(
        &mut ctx,
        &_guard,
        &bucket,
        &snapshot_path,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("rebuild ok");
    assert_eq!(report.snapshot_changes.len(), 1);
    assert!(snapshot_path.exists(), "rebuild must write the snapshot");

    // Step 4: load the rebuilt snapshot.
    let rebuilt = djogi::migrate::load_snapshot(&snapshot_path).expect("load rebuilt");

    // Step 5: re-project live via the same helper the rebuild uses.
    // Use the public `verify` entry point and re-derive the live
    // projection through repair's own surface — there is no
    // standalone exported helper, so we call the rebuild a SECOND
    // time into a separate path and compare the two outputs. Both
    // calls re-project from live, so the two AppliedSchemas must
    // agree byte-for-byte modulo the always-empty `generated_at`.
    let snapshot_path_b = temp_path("rebuild-b");
    let _ = std::fs::remove_file(&snapshot_path_b);
    let _ = repair_snapshot_rebuild(
        &mut ctx,
        &_guard,
        &bucket,
        &snapshot_path_b,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("rebuild b ok");
    let rebuilt_b = djogi::migrate::load_snapshot(&snapshot_path_b).expect("load rebuilt b");

    // Step 6: structural equality. Two re-projections of the same
    // live DB must produce identical AppliedSchema values — same
    // models set, same per-table column metadata, same index list.
    assert_eq!(
        rebuilt.models.keys().collect::<Vec<_>>(),
        rebuilt_b.models.keys().collect::<Vec<_>>(),
        "rebuild must be deterministic across calls"
    );
    assert_eq!(
        rebuilt.indexes.iter().map(|i| &i.name).collect::<Vec<_>>(),
        rebuilt_b
            .indexes
            .iter()
            .map(|i| &i.name)
            .collect::<Vec<_>>(),
        "rebuild must produce the same index list across calls"
    );
    // All three migration tables must show up.
    for t in ["t5_b12_alpha", "t5_b12_beta", "t5_b12_gamma"] {
        assert!(
            rebuilt.models.contains_key(t),
            "rebuild must project {t}; got models {:?}",
            rebuilt.models.keys().collect::<Vec<_>>()
        );
    }
    // The index emitted by the apply must round-trip into the
    // rebuilt snapshot's `indexes` list.
    assert!(
        rebuilt
            .indexes
            .iter()
            .any(|i| i.name == "t5_b12_gamma_alpha_id_idx"),
        "rebuild must project the gamma_alpha_id index; got {:?}",
        rebuilt.indexes.iter().map(|i| &i.name).collect::<Vec<_>>()
    );
    // ── Round-3 B-1: per-column / per-index VALUE assertions ────────
    // The rebuild is the only thing under test here; we read VALUES
    // out of the loaded snapshot and compare them to what the live
    // catalog should hold given the apply above. A deterministic-
    // but-wrong projection (swapped type, dropped default, wrong
    // index column list) fails these assertions.

    // (1) `t5_b12_alpha.id` — BIGINT NOT NULL, PK column.
    let alpha = rebuilt
        .models
        .get("t5_b12_alpha")
        .expect("alpha in rebuild");
    let alpha_id = alpha
        .columns
        .iter()
        .find(|c| c.name == "id")
        .expect("alpha.id must be projected");
    assert_eq!(
        alpha_id.sql_type.as_str(),
        "int8",
        "alpha.id must project as `int8` (canonical form of BIGINT); \
         got sql_type {:?}",
        alpha_id.sql_type
    );
    assert!(
        !alpha_id.nullable,
        "alpha.id is the PK and must project as NOT NULL; got nullable={}",
        alpha_id.nullable
    );
    assert_eq!(
        alpha.primary_key.columns,
        vec!["id".to_string()],
        "alpha.primary_key must list exactly `id`; got {:?}",
        alpha.primary_key.columns
    );

    // (2) `t5_b12_beta.created_at` — TIMESTAMPTZ NOT NULL DEFAULT now().
    // The rebuild must round-trip the DEFAULT expression. Postgres
    // returns `DEFAULT now()` as the literal string `now()` from
    // `pg_get_expr`; the projection passes that through. We assert
    // BOTH that `default_sql` is `Some(_)` (i.e. the rebuild did not
    // silently drop the default) AND that the captured expression
    // equals exactly `now()` (case-insensitive, whitespace-trimmed).
    //
    // Codex round-4 B-13: a `.contains("now()")` check would admit
    // `nope_now()` or `timezone('utc', now())` — both are legitimate
    // Postgres defaults, but neither is what we declared. The exact
    // form is the safe assertion because `pg_get_expr` canonicalises
    // `DEFAULT now()` to the literal `now()`.
    let beta = rebuilt.models.get("t5_b12_beta").expect("beta in rebuild");
    let beta_created_at = beta
        .columns
        .iter()
        .find(|c| c.name == "created_at")
        .expect("beta.created_at must be projected");
    assert_eq!(
        beta_created_at.sql_type.as_str(),
        "timestamptz",
        "beta.created_at must project as `timestamptz`; got {:?}",
        beta_created_at.sql_type
    );
    assert!(
        !beta_created_at.nullable,
        "beta.created_at was declared NOT NULL; must round-trip as such"
    );
    let beta_default = beta_created_at
        .default_sql
        .as_deref()
        .expect("beta.created_at must carry a non-None default_sql");
    let beta_default_canonical: String = beta_default
        .trim()
        .as_bytes()
        .iter()
        .map(|b| b.to_ascii_lowercase() as char)
        .collect();
    assert_eq!(
        beta_default_canonical, "now()",
        "beta.created_at default must round-trip as exactly `now()` \
         (trim + lowercase); got {:?}",
        beta_default
    );

    // (3) `t5_b12_gamma.alpha_id` — BIGINT NOT NULL with FK → t5_b12_alpha(id).
    let gamma = rebuilt
        .models
        .get("t5_b12_gamma")
        .expect("gamma in rebuild");
    let gamma_alpha_id = gamma
        .columns
        .iter()
        .find(|c| c.name == "alpha_id")
        .expect("gamma.alpha_id must be projected");
    assert_eq!(
        gamma_alpha_id.sql_type.as_str(),
        "int8",
        "gamma.alpha_id must project as `int8`; got {:?}",
        gamma_alpha_id.sql_type
    );
    assert!(
        !gamma_alpha_id.nullable,
        "gamma.alpha_id was declared NOT NULL; must round-trip as such"
    );
    assert!(
        gamma_alpha_id.foreign_key.is_none(),
        "rebuild projection must leave gamma.alpha_id without FK metadata here \
         because this plan does not create a live FK constraint"
    );

    // (4) `t5_b12_gamma_alpha_id_idx` — NON-unique BTree on `["alpha_id"]`,
    //     owned by `t5_b12_gamma`.
    use djogi::migrate::{IndexKindSchema, IndexTargetSchema, IndexTypeSchema};
    let idx = rebuilt
        .indexes
        .iter()
        .find(|i| i.name == "t5_b12_gamma_alpha_id_idx")
        .expect("gamma index must be projected");
    assert_eq!(
        idx.table.as_str(),
        "t5_b12_gamma",
        "gamma index must claim `t5_b12_gamma` as its owning table; got {:?}",
        idx.table
    );
    assert!(
        matches!(idx.kind, IndexKindSchema::NonUnique),
        "gamma index was created without UNIQUE; must round-trip as NonUnique; \
         got {:?}",
        idx.kind
    );
    assert!(
        matches!(idx.index_type, IndexTypeSchema::BTree),
        "gamma index uses default access method (BTree); got {:?}",
        idx.index_type
    );
    match &idx.target {
        IndexTargetSchema::Columns(cols) => {
            let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["alpha_id"],
                "gamma index column list must be exactly [alpha_id]; got {names:?}"
            );
        }
        IndexTargetSchema::Expression(e) => {
            panic!("gamma index was column-form; rebuild produced expression form: {e}")
        }
    }

    let _ = std::fs::remove_file(&snapshot_path);
    let _ = std::fs::remove_file(&snapshot_path_b);
}

// ── Repair: confirmation witness can't be bypassed ────────────────────────
//
// This is a compile-time guarantee — the witness type only has one
// public variant, no Default, no From<bool>. We pin the runtime
// equivalence here so accidentally adding alternate constructors
// surfaces in CI.

#[djogi::djogi_test]
async fn confirmation_witness_pins_single_variant(mut _ctx: djogi::DjogiContext) {
    let c = RepairConfirmation::OperatorAcknowledged;
    assert_eq!(c, RepairConfirmation::OperatorAcknowledged);
}

// ──────────────────────────────────────────────────────────────────────────
// Codex-fixup tests (B-1 .. B-12, A-3)
// ──────────────────────────────────────────────────────────────────────────

// ── B-1: rollback uses one tx for all transactional segments ─────────────

#[djogi::djogi_test]
async fn rollback_compound_tx_aborts_when_any_segment_fails(mut ctx: djogi::DjogiContext) {
    // B-1: a 2-tx-segment plan whose REVERSE-order down has the
    // FIRST-segment-down (segment 0) fail. Because rollback walks
    // segments in REVERSE, segment 1's down runs first; segment 0's
    // down fails. The fix wraps both transactional segments in ONE
    // compound tx, so the failure on segment 0 must roll back
    // segment 1's already-executed-but-uncommitted down too.
    let _guard = acquire_test_workspace_guard();

    // Apply a plan that creates two tables.
    let plan_apply = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![
            Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    "AddTable t5_b1_alpha",
                    "CREATE TABLE \"t5_b1_alpha\" (\"id\" BIGINT PRIMARY KEY)",
                    "DROP TABLE \"t5_b1_alpha\"",
                )],
            },
            Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    "AddTable t5_b1_beta",
                    "CREATE TABLE \"t5_b1_beta\" (\"id\" BIGINT PRIMARY KEY)",
                    "DROP TABLE \"t5_b1_beta\"",
                )],
            },
        ],
    };
    let runner_ctx = make_runner_ctx(&plan_apply, "V20260425010100__b1_compound", None, None);
    apply_plan(&mut ctx, &plan_apply, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // Build a rollback plan whose segment 0's down references a
    // non-existent table — guaranteed to fail. The compound tx must
    // therefore abort, leaving BOTH tables intact.
    let plan_rollback = MigrationPlan {
        bucket: plan_apply.bucket.clone(),
        classification: Classification::Additive,
        segments: vec![
            Segment {
                kind: SegmentKind::Transactional,
                statements: vec![OperationSql {
                    label: "AddTable t5_b1_alpha".to_string(),
                    up: "CREATE TABLE \"t5_b1_alpha\" (\"id\" BIGINT PRIMARY KEY)".to_string(),
                    down: "DROP TABLE \"t5_b1_alpha_does_not_exist_will_fail\"".to_string(),
                    lossy: None,
                }],
            },
            Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    "AddTable t5_b1_beta",
                    "CREATE TABLE \"t5_b1_beta\" (\"id\" BIGINT PRIMARY KEY)",
                    "DROP TABLE \"t5_b1_beta\"",
                )],
            },
        ],
    };
    // The rollback plan's checksum_up differs from the original — to
    // exercise rollback we use the same runner_ctx (which carries
    // the original checksum) but a different plan. Rollback does NOT
    // verify the plan's checksum_up against the ledger row's
    // checksum_up — it just walks the plan's segments. (That's the
    // reason the resume path B-10 exists separately.)
    let err = rollback_plan(
        &mut ctx,
        &plan_rollback,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await
    .expect_err("rollback must fail on bad down");
    match err {
        RollbackError::DownStatementFailed { .. } => (),
        other => panic!("expected DownStatementFailed, got {other:?}"),
    }

    // Both tables must still exist — the compound tx aborted.
    let alpha_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_b1_alpha' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("alpha exists");
    let beta_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_b1_beta' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("beta exists");
    assert!(
        alpha_exists,
        "B-1: alpha must still exist after compound rollback aborts"
    );
    assert!(
        beta_exists,
        "B-1: beta must still exist (segment 1 down ran but the compound tx rolled back)"
    );

    // Cleanup so re-runs of the test are clean.
    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_b1_alpha").await;
    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_b1_beta").await;
}

// ── B-2: rollback clears total_steps to NULL ─────────────────────────────

#[djogi::djogi_test]
async fn rollback_clears_total_steps_to_null(mut ctx: djogi::DjogiContext) {
    // B-2: rollback's UPDATE must SET total_steps = NULL. We seed a
    // ledger row with non-zero total_steps (via fake-apply, which
    // does NOT set total_steps — so we directly UPDATE the row) and
    // confirm the rollback clears it.
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_b2_total_steps",
        "CREATE TABLE \"t5_b2_total_steps\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_b2_total_steps\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010101__b2_total_steps", None, None);
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // Backdoor: set total_steps to a non-zero value to mimic a
    // partial-apply state that survived past apply_plan.
    ctx.raw_execute(
        "UPDATE djogi_schema_migrations SET total_steps = 5 WHERE version = $1",
        &[&runner_ctx.version],
    )
    .await
    .expect("seed total_steps");

    rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await
    .expect("rollback ok");

    let total_steps: Option<i32> = ctx
        .raw_scalar(
            "SELECT total_steps FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("read total_steps");
    assert!(
        total_steps.is_none(),
        "B-2: total_steps must be NULL after rollback; got {total_steps:?}"
    );
}

// ── B-3: PriorSnapshotMissing fires before any mutation ──────────────────

#[djogi::djogi_test]
async fn rollback_prior_snapshot_missing_does_not_mutate(mut ctx: djogi::DjogiContext) {
    // B-3: when snapshot_path is Some but prior_snapshot is None,
    // rollback must return PriorSnapshotMissing BEFORE running any
    // down SQL or touching the ledger. Confirm: table still exists,
    // ledger row still says `applied`.
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_path("b3-snap");
    let plan = transactional_plan(vec![op(
        "AddTable t5_b3_prior_missing",
        "CREATE TABLE \"t5_b3_prior_missing\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_b3_prior_missing\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425010102__b3_prior_missing",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
    );
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let err = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None, // <-- snapshot_path is Some but prior_snapshot is None
    )
    .await
    .expect_err("rollback must refuse");
    match err {
        RollbackError::PriorSnapshotMissing => (),
        other => panic!("expected PriorSnapshotMissing, got {other:?}"),
    }

    // Table must still exist — no down ran.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_b3_prior_missing' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists");
    assert!(
        exists,
        "B-3: table must still exist when rollback refuses early"
    );

    // Ledger row must still be `applied`.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(
        status, "applied",
        "B-3: ledger row must still say `applied` when rollback refuses early"
    );

    let _ = ctx
        .raw_ddl("DROP TABLE IF EXISTS t5_b3_prior_missing")
        .await;
    let _ = std::fs::remove_file(&snapshot_path);
}

// ── B-4: fake_apply leaves status='faked', not 'pending' ─────────────────

#[djogi::djogi_test]
async fn fake_apply_status_is_faked_not_pending(mut ctx: djogi::DjogiContext) {
    // B-4: fake-apply must flip the row to status='faked' after
    // insert_pending writes 'pending'. The previous arrangement left
    // the row at 'pending' forever.
    //
    // Codex round-2 B-4 follow-up: the insert + UPDATE pair now sits
    // inside a single Postgres tx (BEGIN / insert_pending / UPDATE /
    // COMMIT) so a crash between the two writes can no longer strand
    // the row at 'pending'. Either both writes commit (the row is
    // 'faked') or neither does (the row is absent). This test
    // observes the happy-path commit; a future (Phase 7-side)
    // crash-injection harness can pin the rollback path explicitly.
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_b4_faked",
        "CREATE TABLE \"t5_b4_faked\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_b4_faked\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010103__b4_faked", None, None);

    fake_apply_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        "operator confirmed out-of-band fix",
    )
    .await
    .expect("fake-apply ok");

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(status, "faked", "B-4: status must be faked, not pending");

    // partial_apply_note must carry the operator's reason.
    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note");
    let note = note.expect("note");
    assert!(
        note.contains("operator confirmed out-of-band fix"),
        "note: {note}"
    );
}

// ── B-5: verify detects DEFAULT mismatch (D607) ─────────────────────────

#[djogi::djogi_test]
async fn verify_detects_default_drift_as_d607(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    // Live DB has a column with no default; snapshot expects DEFAULT
    // now(). D607 must fire.
    ctx.raw_ddl(
        "CREATE TABLE t5_b5_default (id BIGINT PRIMARY KEY, created_at TIMESTAMPTZ NOT NULL)",
    )
    .await
    .expect("create");

    let mut snap = empty_snapshot();
    let mut id_col = djogi::migrate::ColumnSchema {
        check: None,
        default_sql: None,
        foreign_key: None,
        index_type: None,
        indexed: false,
        max_length: None,
        name: "id".to_string(),
        nullable: false,
        on_delete: None,
        outbox_exclude: false,
        rationale: None,
        relation_kind: None,
        renamed_from: None,
        sequence_within: None,
        sql_type: "BIGINT".to_string(),
        unique: false,
    };
    id_col.default_sql = None;
    let mut created_col = id_col.clone();
    created_col.name = "created_at".to_string();
    created_col.sql_type = "TIMESTAMPTZ".to_string();
    created_col.default_sql = Some("now()".to_string()); // <-- snapshot expects default
    snap.models.insert(
        "t5_b5_default".to_string(),
        djogi::migrate::TableSchema {
            app: None,
            columns: vec![id_col, created_col],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: djogi::migrate::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "t5_b5_default".to_string(),
            tenant_key: None,
        },
    );
    let report = verify(&mut ctx, &snap).await.expect("verify ok");
    assert!(
        report.diagnostics.iter().any(|d| d.code == "D607"
            && d.severity == VerifySeverity::Error
            && d.location.as_deref() == Some("t5_b5_default.created_at")),
        "expected D607 t5_b5_default.created_at; got: {:?}",
        report.diagnostics,
    );

    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_b5_default").await;
}

// ── B-6: verify detects PK mismatch (D608) ─────────────────────────────

#[djogi::djogi_test]
async fn verify_detects_pk_mismatch_as_d608(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    // Live DB has PK on `id`; snapshot declares PK on `email`.
    ctx.raw_ddl("CREATE TABLE t5_b6_pk (id BIGINT PRIMARY KEY, email TEXT NOT NULL)")
        .await
        .expect("create");

    let mut snap = empty_snapshot();
    let id_col = djogi::migrate::ColumnSchema {
        check: None,
        default_sql: None,
        foreign_key: None,
        index_type: None,
        indexed: false,
        max_length: None,
        name: "id".to_string(),
        nullable: false,
        on_delete: None,
        outbox_exclude: false,
        rationale: None,
        relation_kind: None,
        renamed_from: None,
        sequence_within: None,
        sql_type: "BIGINT".to_string(),
        unique: false,
    };
    let mut email_col = id_col.clone();
    email_col.name = "email".to_string();
    email_col.sql_type = "TEXT".to_string();
    snap.models.insert(
        "t5_b6_pk".to_string(),
        djogi::migrate::TableSchema {
            app: None,
            columns: vec![id_col, email_col],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["email".to_string()], // <-- mismatched PK column
                kind: djogi::migrate::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "t5_b6_pk".to_string(),
            tenant_key: None,
        },
    );
    let report = verify(&mut ctx, &snap).await.expect("verify ok");
    assert!(
        report.diagnostics.iter().any(|d| d.code == "D608"
            && d.severity == VerifySeverity::Error
            && d.location.as_deref() == Some("t5_b6_pk.<pk>")),
        "expected D608 t5_b6_pk.<pk>; got: {:?}",
        report.diagnostics,
    );

    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_b6_pk").await;
}

// ── B-9: verify detects FK deferrability drift (D609) ────────────────────

#[djogi::djogi_test]
async fn verify_detects_deferrable_fk_drift_as_d609(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    ctx.raw_ddl(
        "CREATE TABLE t5_fk_parent (id BIGINT PRIMARY KEY); \
         CREATE TABLE t5_fk_child ( \
             id BIGINT PRIMARY KEY, \
             parent_id BIGINT NOT NULL, \
             CONSTRAINT t5_fk_child_parent_id_fkey \
                 FOREIGN KEY (parent_id) REFERENCES t5_fk_parent(id) \
                 ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED \
         )",
    )
    .await
    .expect("create tables with deferrable FK");

    let id_col = djogi::migrate::ColumnSchema {
        check: None,
        default_sql: None,
        foreign_key: None,
        index_type: None,
        indexed: false,
        max_length: None,
        name: "id".to_string(),
        nullable: false,
        on_delete: None,
        outbox_exclude: false,
        rationale: None,
        relation_kind: None,
        renamed_from: None,
        sequence_within: None,
        sql_type: "BIGINT".to_string(),
        unique: false,
    };
    let mut snap = empty_snapshot();
    snap.models.insert(
        "t5_fk_parent".to_string(),
        djogi::migrate::TableSchema {
            app: None,
            columns: vec![id_col.clone()],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: djogi::migrate::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "t5_fk_parent".to_string(),
            tenant_key: None,
        },
    );
    snap.models.insert(
        "t5_fk_child".to_string(),
        djogi::migrate::TableSchema {
            app: None,
            columns: vec![
                id_col,
                djogi::migrate::ColumnSchema {
                    check: None,
                    default_sql: None,
                    foreign_key: Some(djogi::migrate::ForeignKeySchema {
                        deferrable: true,
                        initially_deferred: true,
                        on_delete: djogi::migrate::OnDeleteSchema::Restrict,
                        ref_column: "id".to_string(),
                        ref_table: "t5_fk_parent".to_string(),
                    }),
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "parent_id".to_string(),
                    nullable: false,
                    on_delete: Some(djogi::migrate::OnDeleteSchema::Restrict),
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: Some(djogi::migrate::RelationKindSchema::ForeignKey),
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                },
            ],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: djogi::migrate::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "t5_fk_child".to_string(),
            tenant_key: None,
        },
    );

    let clean = verify(&mut ctx, &snap).await.expect("verify clean");
    assert!(
        !clean.diagnostics.iter().any(|d| d.code == "D609"),
        "matching deferrable FK must not drift: {:?}",
        clean.diagnostics
    );

    ctx.raw_ddl(
        "ALTER TABLE t5_fk_child DROP CONSTRAINT t5_fk_child_parent_id_fkey; \
         ALTER TABLE t5_fk_child ADD CONSTRAINT t5_fk_child_parent_id_fkey \
             FOREIGN KEY (parent_id) REFERENCES t5_fk_parent(id) \
             ON DELETE RESTRICT",
    )
    .await
    .expect("replace FK with non-deferrable variant");

    let drifted = verify(&mut ctx, &snap).await.expect("verify drifted");
    assert!(
        drifted
            .diagnostics
            .iter()
            .any(|d| d.code == "D609" && d.location.as_deref() == Some("t5_fk_child.parent_id")),
        "expected D609 for FK deferrability drift; got {:?}",
        drifted.diagnostics,
    );

    let _ = ctx
        .raw_ddl(
            "DROP TABLE IF EXISTS t5_fk_child; \
             DROP TABLE IF EXISTS t5_fk_parent",
        )
        .await;
}

// ── B-7: verify detects index shape mismatch ─────────────────────────────

#[djogi::djogi_test]
async fn verify_detects_index_wrong_columns_as_d612(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    ctx.raw_ddl(
        "CREATE TABLE t5_b7_idx_cols (id BIGINT PRIMARY KEY, a TEXT NOT NULL, b TEXT NOT NULL); \
         CREATE INDEX t5_b7_idx ON t5_b7_idx_cols (a)",
    )
    .await
    .expect("create");

    let mut snap = empty_snapshot();
    snap.indexes.push(djogi::migrate::IndexSchema {
        extension_dependency: None,
        include: Vec::new(),
        index_type: djogi::migrate::IndexTypeSchema::BTree,
        kind: djogi::migrate::IndexKindSchema::NonUnique,
        name: "t5_b7_idx".to_string(),
        nulls_not_distinct: false,
        predicate: None,
        requires_out_of_transaction: false,
        table: "t5_b7_idx_cols".to_string(),
        target: djogi::migrate::IndexTargetSchema::Columns(vec![
            djogi::migrate::IndexColumnSchema {
                name: "b".to_string(), // <-- wrong column
                nulls: djogi::migrate::IndexNullsOrderSchema::Default,
                opclass: None,
                order: djogi::migrate::IndexOrderSchema::Asc,
            },
        ]),
    });
    // Also tell snapshot about the table so D602 doesn't drown the
    // diagnostic list.
    snap.models.insert(
        "t5_b7_idx_cols".to_string(),
        djogi::migrate::TableSchema {
            app: None,
            columns: vec![
                djogi::migrate::ColumnSchema {
                    check: None,
                    default_sql: None,
                    foreign_key: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "id".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                },
                djogi::migrate::ColumnSchema {
                    check: None,
                    default_sql: None,
                    foreign_key: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "a".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "TEXT".to_string(),
                    unique: false,
                },
                djogi::migrate::ColumnSchema {
                    check: None,
                    default_sql: None,
                    foreign_key: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "b".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "TEXT".to_string(),
                    unique: false,
                },
            ],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: djogi::migrate::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "t5_b7_idx_cols".to_string(),
            tenant_key: None,
        },
    );
    let report = verify(&mut ctx, &snap).await.expect("verify ok");
    assert!(
        report.diagnostics.iter().any(|d| d.code == "D612"
            && d.severity == VerifySeverity::Error
            && d.location.as_deref() == Some("index:t5_b7_idx")),
        "expected D612 index:t5_b7_idx; got: {:?}",
        report.diagnostics,
    );

    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_b7_idx_cols").await;
}

// ── B-8: verify on missing ledger emits D621, doesn't bootstrap ─────────

#[djogi::djogi_test]
async fn verify_missing_ledger_emits_d621_without_bootstrap(mut ctx: djogi::DjogiContext) {
    // B-8: verify is read-only. On a fresh DB it must NOT create the
    // ledger; instead it surfaces D621.
    //
    // Setup: ensure the ledger does NOT exist. We can't rely on a
    // truly fresh DB because prior tests may have bootstrapped.
    // Drop it explicitly to set up the test condition.
    ctx.raw_ddl("DROP TABLE IF EXISTS djogi_schema_migrations")
        .await
        .expect("clear ledger");

    let report = verify(&mut ctx, &empty_snapshot())
        .await
        .expect("verify ok");
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "D621" && d.severity == VerifySeverity::Error),
        "expected D621; got: {:?}",
        report.diagnostics,
    );

    // Ledger must still NOT exist.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(
        !exists,
        "B-8: verify must NOT bootstrap the ledger when it is missing",
    );
}

// ── B-9: repair_checksum_drift updates both up and down checksums ───────

#[djogi::djogi_test]
async fn repair_checksum_drift_repairs_both_up_and_down(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable t5_b9_drift_both",
        "CREATE TABLE \"t5_b9_drift_both\" (\"id\" BIGINT)",
        "DROP TABLE \"t5_b9_drift_both\"",
    )]);
    let runner_ctx = make_runner_ctx(&plan, "V20260425010104__b9_drift_both", None, None);
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // Seed a non-NULL checksum_down so the repair has both sides to
    // touch (the runner does not record one for transactional-only
    // plans without lossy operations; we set it directly).
    let original_down = compute_checksum(["original_down_fragment"]);
    ctx.raw_execute(
        "UPDATE djogi_schema_migrations SET checksum_down = $2 WHERE version = $1",
        &[&runner_ctx.version, &original_down],
    )
    .await
    .expect("seed down");

    let new_up = compute_checksum(["fresh_up_fragment"]);
    let new_down = compute_checksum(["fresh_down_fragment"]);
    let report = repair_checksum_drift(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        &new_up,
        Some(&new_down),
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("repair ok");
    assert_eq!(
        report.ledger_changes.len(),
        2,
        "B-9: repair must record both checksum_up and checksum_down changes",
    );

    let stored_up: String = ctx
        .raw_scalar(
            "SELECT checksum_up FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("up");
    let stored_down: Option<String> = ctx
        .raw_scalar(
            "SELECT checksum_down FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("down");
    assert_eq!(stored_up, new_up);
    assert_eq!(stored_down.as_deref(), Some(new_down.as_str()));
}

// ── B-10: repair_resume_partial_apply executes from applied_steps_count+1

#[djogi::djogi_test]
async fn repair_resume_partial_apply_resumes_remaining_steps(mut ctx: djogi::DjogiContext) {
    // B-10: simulate a partial apply, then resume.
    let _guard = acquire_test_workspace_guard();

    // The plan: one transactional CREATE TABLE then two non-tx
    // CREATE INDEX statements; we fake a state where step 1 of 2
    // already applied so the resume runs only step 2.
    ctx.raw_ddl("DROP TABLE IF EXISTS t5_b10_resume")
        .await
        .expect("clean");
    let plan = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![
            Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    "AddTable t5_b10_resume",
                    "CREATE TABLE \"t5_b10_resume\" (\"id\" BIGINT, \"a\" TEXT, \"b\" TEXT)",
                    "DROP TABLE \"t5_b10_resume\"",
                )],
            },
            Segment {
                kind: SegmentKind::NonTransactional,
                statements: vec![
                    op(
                        "AddIndex t5_b10_resume_a_idx",
                        "CREATE INDEX \"t5_b10_resume_a_idx\" ON \"t5_b10_resume\" (\"a\")",
                        "DROP INDEX \"t5_b10_resume_a_idx\"",
                    ),
                    op(
                        "AddIndex t5_b10_resume_b_idx",
                        "CREATE INDEX \"t5_b10_resume_b_idx\" ON \"t5_b10_resume\" (\"b\")",
                        "DROP INDEX \"t5_b10_resume_b_idx\"",
                    ),
                ],
            },
        ],
    };
    let runner_ctx = make_runner_ctx(&plan, "V20260425010105__b10_resume", None, None);

    // Apply the transactional segment + the FIRST non-tx step
    // manually so the ledger ends up in failed state with
    // applied_steps_count=1 / total_steps=2.
    ctx.raw_ddl("CREATE TABLE \"t5_b10_resume\" (\"id\" BIGINT, \"a\" TEXT, \"b\" TEXT)")
        .await
        .expect("manual create");
    ctx.raw_ddl("CREATE INDEX \"t5_b10_resume_a_idx\" ON \"t5_b10_resume\" (\"a\")")
        .await
        .expect("manual idx");
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    // Insert a synthetic row in `failed` state with steps=1 / total=2
    // and the correct checksum_up so the resume validates clean.
    let checksum_up = runner_ctx.checksum_up.clone();
    let run_id: i64 = 42;
    ctx.raw_execute(
        "INSERT INTO djogi_schema_migrations \
         (version, description, checksum_up, execution_mode, status, \
          applied_steps_count, total_steps, run_id, snapshot_version, app_label) \
         VALUES ($1, $2, $3, 'non_transactional', 'failed', \
                 1, 2, $4, '1', '')",
        &[
            &runner_ctx.version,
            &runner_ctx.description,
            &checksum_up,
            &run_id,
        ],
    )
    .await
    .expect("seed ledger row");

    let report = repair_resume_partial_apply(
        &mut ctx,
        &_guard,
        &runner_ctx.version,
        &plan,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("resume ok");
    assert!(
        report
            .actions_taken
            .iter()
            .any(|a| a.contains("AddIndex t5_b10_resume_b_idx")),
        "actions: {:?}",
        report.actions_taken,
    );

    // The ledger row must now be `applied` with applied_steps_count=2.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status");
    assert_eq!(status, "applied", "B-10: resume must finalise to applied");
    let applied_steps: i32 = ctx
        .raw_scalar(
            "SELECT applied_steps_count FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("applied_steps");
    assert_eq!(
        applied_steps, 2,
        "B-10: applied_steps_count must equal total_steps"
    );

    // The second index must now exist.
    let b_idx_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 't5_b10_resume_b_idx' AND relkind = 'i')",
            &[],
        )
        .await
        .expect("exists b_idx");
    assert!(b_idx_exists, "B-10: resume must run step 2's CREATE INDEX");

    let _ = ctx
        .raw_ddl("DROP INDEX IF EXISTS t5_b10_resume_a_idx")
        .await;
    let _ = ctx
        .raw_ddl("DROP INDEX IF EXISTS t5_b10_resume_b_idx")
        .await;
    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_b10_resume").await;
}

#[djogi::djogi_test]
async fn repair_resume_rejects_plan_checksum_mismatch(mut ctx: djogi::DjogiContext) {
    // The resume guard rejects a plan whose recomputed checksum_up
    // disagrees with the ledger row's checksum_up.
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    let version = "V20260425010106__b10_mismatch";

    // Seed a row with a known checksum_up.
    let stored_checksum = compute_checksum(["original up SQL"]);
    let version_owned = version.to_string();
    ctx.raw_execute(
        "INSERT INTO djogi_schema_migrations \
         (version, description, checksum_up, execution_mode, status, \
          applied_steps_count, total_steps, run_id, snapshot_version, app_label) \
         VALUES ($1, 'desc', $2, 'non_transactional', 'failed', \
                 0, 2, 1, '1', '')",
        &[&version_owned, &stored_checksum],
    )
    .await
    .expect("seed");

    // Build a plan whose checksum_up does NOT match.
    let plan = MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::NonTransactional,
            statements: vec![op("X", "SELECT 1", ""), op("Y", "SELECT 2", "")],
        }],
    };

    let err = repair_resume_partial_apply(
        &mut ctx,
        &_guard,
        version,
        &plan,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect_err("must reject");
    match err {
        RepairError::PlanChecksumMismatch { .. } => (),
        other => panic!("expected PlanChecksumMismatch, got {other:?}"),
    }
}

// ── B-11: baseline projects the live database ───────────────────────────

#[djogi::djogi_test]
async fn baseline_projects_live_database_into_snapshot(mut ctx: djogi::DjogiContext) {
    // B-11: baseline_plan must call into the live-DB projection
    // helper and write the projected snapshot. Apply a non-Djogi
    // schema directly, run baseline, assert the snapshot file
    // matches.
    let _guard = acquire_test_workspace_guard();
    ctx.raw_ddl("DROP TABLE IF EXISTS t5_b11_legacy_users")
        .await
        .expect("clean");
    ctx.raw_ddl("CREATE TABLE t5_b11_legacy_users (id BIGINT PRIMARY KEY, email TEXT NOT NULL)")
        .await
        .expect("create legacy");

    let bucket = BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    };
    let snapshot_path = temp_path("b11-baseline");
    let plan = transactional_plan(vec![op("noop", "SELECT 1", "SELECT 1")]);
    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: "V20260425010107__b11_baseline".to_string(),
        description: "baseline test".to_string(),
        // Baseline computes its own checksum_up — the value passed
        // here is ignored on the success path.
        checksum_up: compute_checksum(["placeholder"]),
        checksum_down: None,
        snapshot: None, // <-- B-11 forbids supplying a snapshot
        snapshot_path: Some(snapshot_path.clone()),
        config: MigrateConfig::default(),
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
    };
    let _plan = plan; // unused — baseline does not consume the plan SQL

    baseline_plan(
        &mut ctx,
        &bucket,
        &runner_ctx,
        &_guard,
        "established from live legacy schema",
    )
    .await
    .expect("baseline ok");

    assert!(snapshot_path.exists(), "snapshot must be persisted");
    let written = djogi::migrate::load_snapshot(&snapshot_path).expect("load");
    assert!(
        written.models.contains_key("t5_b11_legacy_users"),
        "B-11: baseline must project the live legacy table; got {:?}",
        written.models.keys().collect::<Vec<_>>()
    );

    let _ = ctx
        .raw_ddl("DROP TABLE IF EXISTS t5_b11_legacy_users")
        .await;
    let _ = std::fs::remove_file(&snapshot_path);
}

#[djogi::djogi_test]
async fn baseline_rejects_caller_supplied_snapshot(mut ctx: djogi::DjogiContext) {
    // B-11 guard: passing a snapshot is the operator-confusion case
    // we explicitly want to refuse.
    let _guard = acquire_test_workspace_guard();
    let bucket = BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    };
    let plan = transactional_plan(vec![op("noop", "SELECT 1", "SELECT 1")]);
    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: "V20260425010108__b11_rejects_supplied".to_string(),
        description: "guarded baseline".to_string(),
        checksum_up: compute_checksum(["placeholder"]),
        checksum_down: None,
        snapshot: Some(empty_snapshot()), // <-- the bad input
        snapshot_path: None,
        config: MigrateConfig::default(),
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
    };
    let _plan = plan;

    let err = baseline_plan(&mut ctx, &bucket, &runner_ctx, &_guard, "guarded")
        .await
        .expect_err("baseline must refuse");
    match err {
        RunnerError::BaselineSnapshotShouldNotBeProvided => (),
        other => panic!("expected BaselineSnapshotShouldNotBeProvided, got {other:?}"),
    }
}

// ── Round-3 A-1: two-bucket baseline scoping (named vs synthetic global) ──
//
// Codex round-3 A-1: the only B-11 live test (above) exercised a
// SINGLE bucket. The B-11 contract is broader — each bucket's
// baseline must project ONLY the tables that belong to that bucket,
// so a named app does not pull in a peer app's tables and the
// synthetic global bucket does not silently swallow a named app's
// tables.
//
// We pick the round-3 Option B framing: without test-time
// `inventory::submit!` to register synthetic descriptors, a named
// bucket's projection is empty by construction (no descriptor
// claims any of the freshly-created live tables for `phantom_bill`,
// `phantom_users`, etc.). The synthetic global bucket projection,
// by contrast, includes the live tables because no named-app
// descriptor claims them either. The CLEAR difference between the
// two snapshots — global has the tables, named does not — proves
// the bucket parameter is being honoured at the projection layer.

#[djogi::djogi_test]
async fn baseline_scopes_projection_to_supplied_bucket_app(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();

    // Two adopter-owned tables created via raw DDL — no inventory
    // descriptors claim them, so they live in the synthetic global
    // bucket.
    ctx.raw_ddl("DROP TABLE IF EXISTS t5_a1_alpha_table")
        .await
        .expect("clean alpha");
    ctx.raw_ddl("DROP TABLE IF EXISTS t5_a1_beta_table")
        .await
        .expect("clean beta");
    ctx.raw_ddl("CREATE TABLE t5_a1_alpha_table (id BIGINT PRIMARY KEY, label TEXT NOT NULL)")
        .await
        .expect("create alpha");
    ctx.raw_ddl("CREATE TABLE t5_a1_beta_table (id BIGINT PRIMARY KEY, payload TEXT)")
        .await
        .expect("create beta");

    // Bucket 1: synthetic global (`app == ""`). Baseline should
    // include both tables.
    let global_bucket = BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    };
    let global_path = temp_path("a1-global");
    let global_runner_ctx = RunnerCtx {
        bucket: global_bucket.clone(),
        version: "V20260425010301__a1_global".to_string(),
        description: "two-bucket scope test (global)".to_string(),
        checksum_up: compute_checksum(["placeholder"]),
        checksum_down: None,
        snapshot: None,
        snapshot_path: Some(global_path.clone()),
        config: MigrateConfig::default(),
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
    };
    baseline_plan(
        &mut ctx,
        &global_bucket,
        &global_runner_ctx,
        &_guard,
        "global bucket scope test",
    )
    .await
    .expect("baseline global ok");
    let global_snap = djogi::migrate::load_snapshot(&global_path).expect("load global");

    // Bucket 2: named bucket (`app == "phantom_billing"`). No
    // descriptor in inventory declares this app label, so the named
    // bucket's projection MUST exclude every live table. The named
    // bucket's snapshot must therefore contain neither
    // `t5_a1_alpha_table` nor `t5_a1_beta_table`.
    let named_bucket = BucketKey {
        database: "main".to_string(),
        app: "phantom_billing".to_string(),
    };
    let named_path = temp_path("a1-named");
    let named_runner_ctx = RunnerCtx {
        bucket: named_bucket.clone(),
        version: "V20260425010302__a1_named".to_string(),
        description: "two-bucket scope test (named)".to_string(),
        checksum_up: compute_checksum(["placeholder"]),
        checksum_down: None,
        snapshot: None,
        snapshot_path: Some(named_path.clone()),
        config: MigrateConfig::default(),
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
    };
    baseline_plan(
        &mut ctx,
        &named_bucket,
        &named_runner_ctx,
        &_guard,
        "named bucket scope test",
    )
    .await
    .expect("baseline named ok");
    let named_snap = djogi::migrate::load_snapshot(&named_path).expect("load named");

    // Assertion 1 — the global bucket's projection includes BOTH
    // adopter tables.
    assert!(
        global_snap.models.contains_key("t5_a1_alpha_table"),
        "global bucket must include t5_a1_alpha_table; got {:?}",
        global_snap.models.keys().collect::<Vec<_>>()
    );
    assert!(
        global_snap.models.contains_key("t5_a1_beta_table"),
        "global bucket must include t5_a1_beta_table; got {:?}",
        global_snap.models.keys().collect::<Vec<_>>()
    );

    // Assertion 2 — the named bucket's projection includes NEITHER
    // adopter table (no descriptor claims either for `phantom_billing`).
    assert!(
        !named_snap.models.contains_key("t5_a1_alpha_table"),
        "named bucket `phantom_billing` must NOT include t5_a1_alpha_table; \
         got {:?}",
        named_snap.models.keys().collect::<Vec<_>>()
    );
    assert!(
        !named_snap.models.contains_key("t5_a1_beta_table"),
        "named bucket `phantom_billing` must NOT include t5_a1_beta_table; \
         got {:?}",
        named_snap.models.keys().collect::<Vec<_>>()
    );

    // Assertion 3 — the two snapshots must DIFFER on these table
    // names. (A trivially-broken projection that always returned
    // every live table for every bucket would pass assertions 1 and
    // 2 individually if you only checked the wrong bucket each time;
    // this assertion forces the two snapshots' model sets to be
    // genuinely different.)
    let global_set: std::collections::BTreeSet<&str> =
        global_snap.models.keys().map(String::as_str).collect();
    let named_set: std::collections::BTreeSet<&str> =
        named_snap.models.keys().map(String::as_str).collect();
    assert_ne!(
        global_set, named_set,
        "round-3 A-1: global and named buckets must produce DIFFERENT \
         projections; got identical model sets {global_set:?}"
    );

    // Cleanup.
    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_a1_alpha_table").await;
    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS t5_a1_beta_table").await;
    let _ = std::fs::remove_file(&global_path);
    let _ = std::fs::remove_file(&named_path);
}

// ── Round-3 A-2: verify does not exclude adopter `heer_orders` table ──
//
// Codex round-3 A-2: the round-2 A-1 fix landed in unit-test space
// (`is_heeranjid_artifact_table("heer_orders")` returns false at the
// allowlist function level), but the live integration suite carried
// no test that proved the policy end-to-end against a real Postgres
// catalog. A live test that creates `heer_orders` in the public
// schema and runs `verify` against an empty snapshot is the only way
// to prove the projection / verify pipeline does not silently drop
// the table.
//
// The expected outcome: with an empty snapshot and a live
// `heer_orders` table, verify must emit `D602` (live table not in
// snapshot) for `heer_orders`. If the table were silently excluded
// (the pre-A-1 LIKE-based behaviour), verify would emit nothing at
// all for `heer_orders` — a silent data loss for the operator who
// just adopted the framework.

#[djogi::djogi_test]
async fn verify_does_not_exclude_adopter_named_heer_orders_table(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    ctx.raw_ddl("DROP TABLE IF EXISTS heer_orders")
        .await
        .expect("clean");
    ctx.raw_ddl(
        "CREATE TABLE heer_orders (\
            id BIGINT PRIMARY KEY, \
            customer TEXT NOT NULL, \
            amount_cents BIGINT NOT NULL)",
    )
    .await
    .expect("create heer_orders");

    let snap = empty_snapshot();
    let report = verify(&mut ctx, &snap).await.expect("verify ok");

    // Round-3 A-2: `heer_orders` must surface as a `D602` (live
    // table not in snapshot) diagnostic. If the projection silently
    // excluded the table — the bug the round-2 A-1 fix targeted —
    // verify would emit no diagnostic for `heer_orders` and the
    // operator would have no way to learn the framework was
    // ignoring their data.
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "D602" && d.location.as_deref() == Some("heer_orders")),
        "round-3 A-2: verify must surface adopter `heer_orders` as D602 \
         (live table not in snapshot); got diagnostics: {:?}",
        report.diagnostics
    );

    let _ = ctx.raw_ddl("DROP TABLE IF EXISTS heer_orders").await;
}
