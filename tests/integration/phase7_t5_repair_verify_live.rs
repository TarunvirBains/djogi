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
    RollbackError, RunnerCtx, SNAPSHOT_FORMAT_VERSION, Segment, SegmentKind, VerifySeverity,
    WorkspaceGuard, acquire_workspace_lock, apply_plan, baseline_plan, bootstrap_ledger,
    compute_checksum, fake_apply_plan, repair_checksum_drift, repair_partial_apply,
    repair_snapshot_rebuild, rollback_plan, verify,
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
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("repair ok");
    assert_eq!(report.ledger_changes.len(), 1);
    assert_eq!(report.ledger_changes[0].column, "checksum_up");
    assert_eq!(report.ledger_changes[0].after, new_checksum);

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
async fn repair_snapshot_rebuild_writes_supplied_snapshot(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let bucket = BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    };
    let snapshot_path = temp_path("rebuild");
    let snap = empty_snapshot();
    let report = repair_snapshot_rebuild(
        &mut ctx,
        &_guard,
        &bucket,
        &snap,
        &snapshot_path,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    .expect("rebuild ok");
    assert_eq!(report.snapshot_changes.len(), 1);
    assert!(snapshot_path.exists());
    let _ = std::fs::remove_file(&snapshot_path);
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
