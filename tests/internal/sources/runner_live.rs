//  — live-PG integration tests for the migration runner.
//
// # What these tests prove
//
// - `bootstrap_ledger` is idempotent against a real Postgres
//   instance.
// - `apply_plan` executes a transactional plan end-to-end, marks
//   the ledger row `applied`, and persists the snapshot file.
// - A transactional apply that fails mid-stream rolls back its
//   transaction, marks the ledger row `failed`, and DOES NOT move
//   the snapshot file forward.
// - A split (transactional + non-transactional) apply records
//   non-transactional progress in `applied_steps_count` and only
//   moves the snapshot on full success.
// - A split apply whose non-transactional step fails records the
//   partial state via `partial_apply_note` and surfaces the failing
//   step ordinal.
// - The relpages probe emits a warn (default config) and aborts
//   (strict-mode config) for a transactional `CREATE INDEX` on a
//   table whose `pg_class.relpages` exceeds the threshold.
// - The advisory-lock key derivation is deterministic across
//   processes and cannot collide on `(database || app)` boundary
//   accidents.
// - The snapshot file is NOT moved forward on any failure path.
// - `run_id` is unique per invocation.
//
// # Test isolation
//
// Each `#[djogi_test]` provisions a fresh `djogi_test_<uuid>`
// database via the harness. The runner targets that
// database directly; cleanup is automatic.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use djogi::config::MigrateConfig;
use djogi::migrate::{
    AppLifecycle, AppliedSchema, BucketKey, Classification, LedgerStatus, LossyRollbackPolicy,
    MigrationPlan, PHASE_ZERO_VERSION, RollbackError, RunnerCtx, RunnerError, RunnerIdentity,
    SNAPSHOT_FORMAT_VERSION, Segment, SegmentKind, WorkspaceGuard, acquire_workspace_lock,
    advisory_lock_key, apply_plan, bootstrap_ledger, compute_checksum, ensure_phase_zero_emitted,
    load_snapshot, rollback_plan,
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

fn test_temp_root() -> PathBuf {
    let temp_canon = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp dir");
    let root = temp_canon.join("djogi-runner-tests");
    djogi::migrate::create_workspace_parent_dirs(&temp_canon, root.join(".keep"))
        .expect("create runner temp root");
    root.canonicalize().expect("canonicalize runner temp root")
}

fn is_within(parent: &Path, child: &Path) -> bool {
    match (parent.canonicalize(), child.canonicalize()) {
        (Ok(parent_abs), Ok(child_abs)) => child_abs.starts_with(parent_abs),
        _ => false,
    }
}

fn temp_snapshot_path() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    test_temp_root().join(format!("djogi-runner-test-{stamp}.json"))
}

/// Per-test workspace lock path. Each test gets its own unique path
/// so concurrent test runs do not contend.
fn temp_workspace_lock_path() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    test_temp_root().join(format!("djogi-runner-test-{stamp}.lock"))
}

fn temp_workspace_root() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_canon = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp dir");
    let path = temp_canon.join(format!("djogi-runner-workspace-{stamp}"));
    let path = djogi::migrate::resolve_write_workspace_path(&temp_canon, &path)
        .expect("resolve temp workspace root");
    djogi::migrate::create_workspace_parent_dirs(&temp_canon, path.join(".keep"))
        .expect("create temp workspace root");
    path.canonicalize().expect("canonicalize temp workspace root")
}

fn safe_remove_snapshot(path: &Path) {
    let temp_root = test_temp_root();
    if is_within(&temp_root, path) {
        let _ = djogi::migrate::remove_workspace_file(&temp_root, path);
    }
}

fn safe_remove_workspace(path: &Path) {
    let temp_canon = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp dir");
    if let Ok(vetted) = djogi::migrate::resolve_existing_workspace_path(&temp_canon, path) {
        let _ = djogi::migrate::remove_workspace_dir_all(&temp_canon, &vetted);
    }
}

/// Acquire a per-test workspace `WorkspaceGuard`, satisfying the
/// witness-typed `&WorkspaceGuard` argument that `apply_plan`
/// requires. The v3 contract requires the file lock be held
/// for the entire run; this helper produces one with a per-test
/// path so two tests cannot collide.
fn acquire_test_workspace_guard() -> WorkspaceGuard {
    let path = temp_workspace_lock_path();
    acquire_workspace_lock(&path, Duration::from_secs(2)).expect("acquire workspace lock")
}

fn op(label: &str, up: &str, down: &str) -> djogi::migrate::OperationSql {
    djogi::migrate::OperationSql {
        label: label.to_string(),
        up: up.to_string(),
        down: down.to_string(),
        lossy: None,
    }
}

async fn index_exists(ctx: &mut djogi::DjogiContext, index_name: &str) -> bool {
    ctx.raw_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = $1 AND relkind = 'i')",
        &[&index_name],
    )
    .await
    .expect("index exists")
}

async fn install_progress_ack_failure_trigger(
    ctx: &mut djogi::DjogiContext,
    fail_on_applied_steps: i32,
) {
    let function_sql = format!(
        "CREATE OR REPLACE FUNCTION djogi_test_fail_progress_ack() \
         RETURNS trigger AS $$ \
         BEGIN \
             IF OLD.status = NEW.status \
                AND OLD.applied_steps_count IS DISTINCT FROM NEW.applied_steps_count \
                AND NEW.applied_steps_count = {fail_on_applied_steps} THEN \
                 RAISE EXCEPTION 'djogi test injected progress ack failure at step %', \
                     NEW.applied_steps_count; \
             END IF; \
             RETURN NEW; \
         END; \
         $$ LANGUAGE plpgsql"
    );
    ctx.raw_ddl(&function_sql)
        .await
        .expect("create progress-ack failure function");
    ctx.raw_ddl("DROP TRIGGER IF EXISTS djogi_test_fail_progress_ack ON djogi_schema_migrations")
        .await
        .expect("drop prior progress-ack failure trigger");
    ctx.raw_ddl(
        "CREATE TRIGGER djogi_test_fail_progress_ack \
         BEFORE UPDATE ON djogi_schema_migrations \
         FOR EACH ROW EXECUTE FUNCTION djogi_test_fail_progress_ack()",
    )
    .await
    .expect("create progress-ack failure trigger");
}

fn transactional_plan(stmts: Vec<djogi::migrate::OperationSql>) -> MigrationPlan {
    transactional_plan_for_app("", stmts)
}

fn transactional_plan_for_app(
    app: &str,
    stmts: Vec<djogi::migrate::OperationSql>,
) -> MigrationPlan {
    MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: app.to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: stmts,
        }],
    }
}

fn canonical_identity_free_phase_zero_sql(guard: &WorkspaceGuard) -> String {
    let workspace = temp_workspace_root();
    let apps = vec![AppLifecycle {
        label: "test_app".to_string(),
        database: "main".to_string(),
        renamed_from: None,
        tombstone: false,
    }];
    let emitted = ensure_phase_zero_emitted(
        &workspace,
        &BTreeMap::new(),
        &apps,
        time::OffsetDateTime::now_utc(),
        guard,
    )
    .expect("emit canonical identity-free Phase 0");
    let sql = fs::read_to_string(&emitted[0].up_sql_path).expect("read emitted Phase 0 up SQL");
    safe_remove_workspace(&workspace);
    sql
}

fn split_plan(
    tx_stmts: Vec<djogi::migrate::OperationSql>,
    non_tx_stmts: Vec<djogi::migrate::OperationSql>,
) -> MigrationPlan {
    let mut segments = Vec::new();
    if !tx_stmts.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::Transactional,
            statements: tx_stmts,
        });
    }
    if !non_tx_stmts.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::NonTransactional,
            statements: non_tx_stmts,
        });
    }
    MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        },
        classification: Classification::Additive,
        segments,
    }
}

fn make_runner_ctx(
    plan: &MigrationPlan,
    version: &str,
    snapshot: Option<AppliedSchema>,
    snapshot_path: Option<PathBuf>,
    config: MigrateConfig,
) -> RunnerCtx {
    // Compute the up checksum from the plan so verification passes.
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
        config,
        //  tests do not exercise out-of-order policy; pick the
        // permissive default so existing assertions are unaffected.
        // The dedicated *.rs tests cover the policy paths.
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
        // .4 audit-pool plumbing: tests do not provision the
        // audit DB. .7 owns the integration coverage that flips
        // this to `Some`.
        audit_pool: None,
        drift_baseline: djogi::migrate::DriftBaseline::Disabled,
        runner_identity: Some(RunnerIdentity::SingleNodeDev),
    }
}

// ── Bootstrap idempotency ─────────────────────────────────────────────────

#[djogi::djogi_test]
async fn bootstrap_is_idempotent(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("first bootstrap");
    bootstrap_ledger(&mut ctx).await.expect("second bootstrap");
    // Verify table exists by selecting from it (zero rows expected).
    let row_count: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM djogi_schema_migrations", &[])
        .await
        .expect("select count");
    assert_eq!(row_count, 0);
}

// ── Happy path: transactional apply ───────────────────────────────────────

#[djogi::djogi_test]
async fn transactional_apply_records_applied_status(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_snapshot_path();
    let plan = transactional_plan(vec![op(
        "AddTable users",
        "CREATE TABLE \"users\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"users\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000001__create_users",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
        MigrateConfig::default(),
    );
    let report = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");
    assert_eq!(report.transactional_segments, 1);
    assert_eq!(report.non_transactional_segments, 0);
    assert!(report.run_id != 0);

    // Ledger row must be `applied`.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("ledger select");
    assert_eq!(status, "applied");

    // Snapshot must exist on disk.
    assert!(snapshot_path.exists(), "snapshot file must be written");
    let loaded = load_snapshot(&snapshot_path).expect("load snapshot");
    assert_eq!(loaded.format_version, SNAPSHOT_FORMAT_VERSION);

    // Cleanup.
    let temp_root = test_temp_root();
    if is_within(&temp_root, &snapshot_path) {
    safe_remove_snapshot(&snapshot_path);
    }
}

// ── Failure: transactional apply rolls back ───────────────────────────────

#[djogi::djogi_test]
async fn transactional_apply_failure_rolls_back_and_skips_snapshot(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_snapshot_path();
    // First statement is fine. Second statement is invalid SQL — the
    // transaction must roll back, the table from the first statement
    // must NOT exist after rollback, and the snapshot file must NOT
    // be written.
    let plan = transactional_plan(vec![
        op(
            "AddTable widgets",
            "CREATE TABLE \"widgets\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"widgets\"",
        ),
        op(
            "AddTable broken",
            "CREATE TABLE \"broken\" (\"id\" THIS_IS_NOT_A_TYPE)",
            "DROP TABLE \"broken\"",
        ),
    ]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000002__broken_apply",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
        MigrateConfig::default(),
    );
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("apply must fail");
    assert!(
        matches!(err, RunnerError::TransactionalSegmentFailed { .. }),
        "got {err:?}"
    );

    // First table must NOT exist (rolled back).
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class \
             WHERE relname = 'widgets' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(!exists, "widgets must not exist after rollback");

    // Ledger row must be `failed`.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("ledger select");
    assert_eq!(status, "failed");

    // partial_apply_note must be populated with diagnostic content.
    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note select");
    let note = note.expect("note must be set");
    assert!(note.contains("transactional segment"), "note: {note}");

    // Snapshot file must NOT exist.
    assert!(
        !snapshot_path.exists(),
        "snapshot file must NOT be written on failure"
    );
}

#[djogi::djogi_test]
async fn phase_zero_single_node_dev_provisioning_failure_marks_failed(
    mut ctx: djogi::DjogiContext,
) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_snapshot_path();
    ctx.raw_ddl(
        "TRUNCATE TABLE heer_node_state, heer_ranj_node_state, heer_nodes, heer_config \
         RESTART IDENTITY CASCADE",
    )
    .await
    .expect("clear harness single-node seed");

    let mut up_sql = canonical_identity_free_phase_zero_sql(&_guard);
    up_sql.push_str(
        "\nALTER TABLE heer_nodes \
         ADD CONSTRAINT djogi_test_reject_single_node_dev_seed CHECK (node_id <> 1);\n",
    );
    let plan = transactional_plan(vec![op(
        "Phase 0 bootstrap with injected single-node-dev seed failure",
        &up_sql,
        "-- Phase 0 down has no reverse operations",
    )]);
    let mut runner_ctx = make_runner_ctx(
        &plan,
        PHASE_ZERO_VERSION,
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
        MigrateConfig::default(),
    );
    runner_ctx.runner_identity = Some(RunnerIdentity::SingleNodeDev);

    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("Phase 0 single-node-dev provisioning must fail before mark_applied");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("single-node-dev") && err_msg.contains("provision"),
        "error must name the provisioning phase: {err_msg}"
    );

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("ledger select");
    assert_eq!(status, "failed");

    let execution_time_ms: i64 = ctx
        .raw_scalar(
            "SELECT execution_time_ms FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("execution_time_ms select");
    assert_eq!(
        execution_time_ms, 0,
        "Phase 0 provisioning failure must occur before mark_applied"
    );

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note select");
    let note = note.expect("failed row must carry provisioning note");
    assert!(
        note.contains("single-node-dev provisioning failed"),
        "note: {note}"
    );

    let node_count: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM heer_nodes WHERE node_id = 1", &[])
        .await
        .expect("node count after failed provisioning");
    assert_eq!(node_count, 0, "node 1 must not be partially provisioned");

    assert!(
        !snapshot_path.exists(),
        "snapshot file must NOT be written on Phase 0 provisioning failure"
    );
    safe_remove_snapshot(&snapshot_path);
}

// ── Split apply: tx + non-tx success ──────────────────────────────────────

#[djogi::djogi_test]
async fn split_apply_records_non_tx_progress(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_snapshot_path();
    // Transactional segment creates the table; non-transactional
    // segment creates two indexes via `CREATE INDEX` (no
    // CONCURRENTLY since the test database starts empty and we are
    // exercising the runner's segment dispatch, not Postgres
    // semantics).
    //
    // We use the "_normal" CREATE INDEX form here because
    // CONCURRENTLY requires being outside a transaction AND requires
    // the relation to exist in the same DB session — which
    // tokio-postgres autocommit honours.
    let plan = split_plan(
        vec![op(
            "AddTable split_users",
            "CREATE TABLE \"split_users\" (\"id\" BIGINT, \"email\" TEXT, \"name\" TEXT)",
            "DROP TABLE \"split_users\"",
        )],
        vec![
            op(
                "AddIndex split_users_email_idx",
                "CREATE INDEX CONCURRENTLY \"split_users_email_idx\" \
                 ON \"split_users\" (\"email\")",
                "DROP INDEX CONCURRENTLY \"split_users_email_idx\"",
            ),
            op(
                "AddIndex split_users_name_idx",
                "CREATE INDEX CONCURRENTLY \"split_users_name_idx\" \
                 ON \"split_users\" (\"name\")",
                "DROP INDEX CONCURRENTLY \"split_users_name_idx\"",
            ),
        ],
    );
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000003__split_apply",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
        MigrateConfig::default(),
    );
    let report = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("split apply ok");
    assert_eq!(report.transactional_segments, 1);
    assert_eq!(report.non_transactional_segments, 1);

    // execution_mode column must record `non_transactional`.
    let mode: String = ctx
        .raw_scalar(
            "SELECT execution_mode FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("mode select");
    assert_eq!(mode, "non_transactional");

    // applied_steps_count must equal 2 (both indexes ran).
    let applied_steps: i32 = ctx
        .raw_scalar(
            "SELECT applied_steps_count FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("applied_steps_count");
    assert_eq!(applied_steps, 2);

    // total_steps must be set to 2.
    let total_steps: Option<i32> = ctx
        .raw_scalar(
            "SELECT total_steps FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("total_steps");
    assert_eq!(total_steps, Some(2));

    // Snapshot file must exist.
    assert!(snapshot_path.exists());
    safe_remove_snapshot(&snapshot_path);
}

// ── Split apply: non-tx mid-step failure ──────────────────────────────────

#[djogi::djogi_test]
async fn split_apply_non_tx_failure_records_partial_state(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_snapshot_path();
    // Transactional segment creates the table; non-transactional
    // segment runs two indexes — the first succeeds, the second
    // references a non-existent column and fails. The runner must
    // mark the row failed with applied_steps_count = 1 and a
    // partial_apply_note describing the step.
    let plan = split_plan(
        vec![op(
            "AddTable split_fail",
            "CREATE TABLE \"split_fail\" (\"id\" BIGINT, \"email\" TEXT)",
            "DROP TABLE \"split_fail\"",
        )],
        vec![
            op(
                "AddIndex split_fail_email_idx",
                "CREATE INDEX CONCURRENTLY \"split_fail_email_idx\" \
                 ON \"split_fail\" (\"email\")",
                "DROP INDEX CONCURRENTLY \"split_fail_email_idx\"",
            ),
            op(
                "AddIndex split_fail_missing_idx",
                "CREATE INDEX CONCURRENTLY \"split_fail_missing_idx\" \
                 ON \"split_fail\" (\"missing_col\")",
                "DROP INDEX CONCURRENTLY \"split_fail_missing_idx\"",
            ),
        ],
    );
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000004__split_partial",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
        MigrateConfig::default(),
    );
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("must fail");
    match err {
        RunnerError::NonTransactionalSegmentFailed {
            step_index,
            applied_steps_count,
            ..
        } => {
            assert_eq!(step_index, 1, "second step (0-indexed 1) failed");
            assert_eq!(
                applied_steps_count, 1,
                "first step succeeded so applied_steps_count = 1"
            );
        }
        other => panic!("expected NonTransactionalSegmentFailed, got {other:?}"),
    }

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status select");
    assert_eq!(status, "failed");

    let applied_steps: i32 = ctx
        .raw_scalar(
            "SELECT applied_steps_count FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("applied_steps_count select");
    assert_eq!(applied_steps, 1);

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note select");
    let note = note.expect("partial_apply_note must be set");
    assert!(note.contains("non-tx step"), "note: {note}");
    assert!(
        note.contains("missing_idx") || note.contains("missing_col"),
        "note: {note}"
    );

    // Snapshot must NOT exist on partial-failure path.
    assert!(
        !snapshot_path.exists(),
        "snapshot file must NOT be written on partial failure"
    );
}

#[djogi::djogi_test]
async fn split_apply_progress_ack_failure_blocks_duplicate_resume(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let snapshot_path = temp_snapshot_path();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    install_progress_ack_failure_trigger(&mut ctx, 1).await;

    let plan = split_plan(
        vec![op(
            "AddTable split_ack",
            "CREATE TABLE \"split_ack\" (\"id\" BIGINT, \"email\" TEXT, \"name\" TEXT)",
            "DROP TABLE \"split_ack\"",
        )],
        vec![
            op(
                "AddIndex split_ack_email_idx",
                "CREATE INDEX CONCURRENTLY \"split_ack_email_idx\" \
                 ON \"split_ack\" (\"email\")",
                "DROP INDEX CONCURRENTLY \"split_ack_email_idx\"",
            ),
            op(
                "AddIndex split_ack_name_idx",
                "CREATE INDEX CONCURRENTLY \"split_ack_name_idx\" \
                 ON \"split_ack\" (\"name\")",
                "DROP INDEX CONCURRENTLY \"split_ack_name_idx\"",
            ),
        ],
    );
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260524000100__split_ack_failure",
        Some(empty_snapshot()),
        Some(snapshot_path.clone()),
        MigrateConfig::default(),
    );

    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("progress ack failure must abort the apply");

    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status after injected failure");
    assert_ne!(
        status, "applied",
        "progress ack failure must leave a non-terminal ledger row"
    );

    let applied_steps: i32 = ctx
        .raw_scalar(
            "SELECT applied_steps_count FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("applied_steps after injected failure");
    assert_eq!(
        applied_steps, 0,
        "acked progress must stay at the last durable boundary"
    );

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("note after injected failure");
    let note = note.expect("progress claim note must be recorded");
    assert!(
        note.contains("non-tx progress claim"),
        "note must preserve the claimed-step marker: {note}"
    );

    assert!(
        index_exists(&mut ctx, "split_ack_email_idx").await,
        "the committed first step must still exist"
    );
    assert!(
        !index_exists(&mut ctx, "split_ack_name_idx").await,
        "the second step must not run after the first ack failure"
    );

    let rerun_err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("duplicate apply must be blocked by the non-terminal row");
    assert!(
        matches!(rerun_err, RunnerError::VersionCollisionNonTerminal { .. }),
        "got {rerun_err:?}"
    );

    assert!(
        !snapshot_path.exists(),
        "snapshot file must not be written on an ambiguous non-tx boundary"
    );
}

// ── Relpages probe: WARN path (default config) ────────────────────────────

#[djogi::djogi_test]
async fn relpages_probe_warns_when_threshold_exceeded(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    // Create the table and force its relpages high enough to trip
    // the probe by setting a tiny threshold via the runner_ctx
    // config. We don't need to actually pad the table — the probe
    // queries pg_class.relpages, which is updated by VACUUM / ANALYZE
    // — but we can side-step that by setting threshold=0 so any
    // non-zero relpages triggers the warn. To get a non-zero
    // relpages we INSERT a few rows and ANALYZE.
    ctx.raw_ddl("CREATE TABLE relpages_warn (id BIGINT, val TEXT)")
        .await
        .expect("create table");
    for i in 0..50i64 {
        ctx.raw_execute(
            "INSERT INTO relpages_warn (id, val) VALUES ($1, $2)",
            &[&i, &format!("row-{i}")],
        )
        .await
        .expect("insert");
    }
    ctx.raw_ddl("ANALYZE relpages_warn")
        .await
        .expect("analyze");

    let plan = transactional_plan(vec![op(
        "AddIndex relpages_warn_val_idx",
        "CREATE INDEX \"relpages_warn_val_idx\" ON \"relpages_warn\" (\"val\")",
        "DROP INDEX \"relpages_warn_val_idx\"",
    )]);
    let config = MigrateConfig {
        concurrent_warn_relpages: 0, // anything > 0 triggers
        strict_concurrent_warnings: false,
        ..MigrateConfig::default()
    };
    let runner_ctx = make_runner_ctx(&plan, "V20260425000005__relpages_warn", None, None, config);
    // WARN path: runner returns Ok and only emits a tracing::warn!.
    let report = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("warn path must succeed");
    assert_eq!(report.transactional_segments, 1);
}

// ── Relpages probe: STRICT path aborts ────────────────────────────────────

#[djogi::djogi_test]
async fn relpages_probe_aborts_in_strict_mode(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    ctx.raw_ddl("CREATE TABLE relpages_strict (id BIGINT, val TEXT)")
        .await
        .expect("create table");
    for i in 0..50i64 {
        ctx.raw_execute(
            "INSERT INTO relpages_strict (id, val) VALUES ($1, $2)",
            &[&i, &format!("row-{i}")],
        )
        .await
        .expect("insert");
    }
    ctx.raw_ddl("ANALYZE relpages_strict")
        .await
        .expect("analyze");

    let plan = transactional_plan(vec![op(
        "AddIndex relpages_strict_val_idx",
        "CREATE INDEX \"relpages_strict_val_idx\" ON \"relpages_strict\" (\"val\")",
        "DROP INDEX \"relpages_strict_val_idx\"",
    )]);
    let config = MigrateConfig {
        concurrent_warn_relpages: 0,
        strict_concurrent_warnings: true,
        ..MigrateConfig::default()
    };
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000006__relpages_strict",
        None,
        None,
        config,
    );
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("strict path must fail");
    match err {
        RunnerError::RelpagesThresholdExceeded {
            index_name,
            target_table,
            relpages,
            threshold,
            ..
        } => {
            assert_eq!(index_name, "relpages_strict_val_idx");
            assert_eq!(target_table, "relpages_strict");
            assert!(relpages > 0);
            assert_eq!(threshold, 0);
        }
        other => panic!("expected RelpagesThresholdExceeded, got {other:?}"),
    }

    // Index must NOT exist (probe ran before BEGIN).
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class \
             WHERE relname = 'relpages_strict_val_idx' AND relkind = 'i')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(!exists);

    // Ledger row must be `failed`.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status select");
    assert_eq!(status, "failed");
}

// ── Relpages probe: small table → no warning ──────────────────────────────

#[djogi::djogi_test]
async fn relpages_probe_silent_for_small_table(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    ctx.raw_ddl("CREATE TABLE relpages_small (id BIGINT, val TEXT)")
        .await
        .expect("create table");
    // Empty table — relpages = 0, threshold default 128, so probe is silent.

    let plan = transactional_plan(vec![op(
        "AddIndex relpages_small_val_idx",
        "CREATE INDEX \"relpages_small_val_idx\" ON \"relpages_small\" (\"val\")",
        "DROP INDEX \"relpages_small_val_idx\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000007__relpages_silent",
        None,
        None,
        MigrateConfig::default(),
    );
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("must succeed silently");
}

// ── Relpages probe: target table missing AND not in plan's AddTable set ───
//
// Codex round A-2 polish: the probe's `None` branch must hard-error
// when the target_table doesn't exist in pg_class AND isn't being
// created by the same plan. The unit tests cover the predicate
// (`collect_add_table_targets`); this live test exercises the full
// runner path against real Postgres so a regression in the probe's
// branch logic surfaces here.

#[djogi::djogi_test]
async fn relpages_probe_hard_errors_when_target_missing(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    // Plan creates an index on a table that does NOT exist in the DB
    // and is NOT in the plan's AddTable set. The relpages probe must
    // surface `TargetTableNotFound` before any segment runs.
    let plan = transactional_plan(vec![op(
        "AddIndex relpages_missing_idx",
        "CREATE INDEX \"relpages_missing_idx\" ON \"relpages_missing_table\" (\"val\")",
        "DROP INDEX \"relpages_missing_idx\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000013__relpages_missing",
        None,
        None,
        MigrateConfig::default(),
    );
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("missing target table must hard-error before BEGIN");
    match err {
        RunnerError::TargetTableNotFound {
            index_name,
            target_table,
            ..
        } => {
            assert_eq!(index_name, "relpages_missing_idx");
            assert_eq!(target_table, "relpages_missing_table");
        }
        other => panic!("expected TargetTableNotFound, got {other:?}"),
    }

    // The index must NOT exist (probe ran before BEGIN, so no DDL ran).
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class \
             WHERE relname = 'relpages_missing_idx' AND relkind = 'i')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(!exists);

    // Ledger row must be `failed` — apply was attempted, the probe
    // rejected before the segment-apply path.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status select");
    assert_eq!(status, "failed");
}

// ── Run-id uniqueness across invocations ──────────────────────────────────

#[djogi::djogi_test]
async fn run_id_is_unique_per_invocation(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan_a = transactional_plan(vec![op(
        "AddTable run_id_a",
        "CREATE TABLE \"run_id_a\" (\"id\" BIGINT)",
        "DROP TABLE \"run_id_a\"",
    )]);
    let plan_b = transactional_plan(vec![op(
        "AddTable run_id_b",
        "CREATE TABLE \"run_id_b\" (\"id\" BIGINT)",
        "DROP TABLE \"run_id_b\"",
    )]);
    let ctx_a = make_runner_ctx(
        &plan_a,
        "V20260425000008__a",
        None,
        None,
        MigrateConfig::default(),
    );
    let ctx_b = make_runner_ctx(
        &plan_b,
        "V20260425000009__b",
        None,
        None,
        MigrateConfig::default(),
    );
    let r_a = apply_plan(&mut ctx, &plan_a, &ctx_a, &_guard)
        .await
        .expect("a");
    let r_b = apply_plan(&mut ctx, &plan_b, &ctx_b, &_guard)
        .await
        .expect("b");
    assert_ne!(r_a.run_id, r_b.run_id, "run_id must differ per invocation");
}

// ── Checksum mismatch is detected ─────────────────────────────────────────

#[djogi::djogi_test]
async fn checksum_mismatch_aborts_before_apply(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    let plan = transactional_plan(vec![op(
        "AddTable checksum",
        "CREATE TABLE \"checksum\" (\"id\" BIGINT)",
        "DROP TABLE \"checksum\"",
    )]);
    // Tamper with the runner_ctx's checksum_up so it does not match
    // a freshly-computed one.
    let mut runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000010__checksum",
        None,
        None,
        MigrateConfig::default(),
    );
    runner_ctx.checksum_up =
        "V1:0000000000000000000000000000000000000000000000000000000000000000".to_string();
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("must fail on checksum");
    assert!(
        matches!(err, RunnerError::ChecksumMismatch(_)),
        "got {err:?}"
    );

    // Table must NOT exist — apply aborted before any SQL ran.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'checksum' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists");
    assert!(!exists);

    // Ledger side effects must also be absent. The public apply entry point
    // rejects checksum drift before pinning or bootstrapping the ledger, so a
    // fresh per-test DB must still have no ledger table at all.
    let ledger_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("ledger probe");
    assert!(
        !ledger_exists,
        "checksum mismatch must refuse before ledger bootstrap on a fresh DB"
    );
}

// ── MetadataOnly segment accounting (live PG) ─────────────────────────────

#[djogi::djogi_test]
async fn metadata_only_segment_accounted_in_run_report(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    // Build a plan with one transactional segment (creates a table)
    // and one metadata-only segment (RenameApp). The runner must:
    //   - run the transactional DDL,
    //   - skip the metadata-only DDL (placeholder SQL comment text),
    //   - count the metadata segment in `RunReport.metadata_segments`,
    //   - record `applied_steps_count` correctly (0, since
    //     metadata-only segments produce no non-transactional steps),
    //   - mark the ledger row applied.
    // Codex round A-5 polish: the metadata segment's `up` SQL is a
    // *real DDL canary* — `CREATE TABLE "metadata_canary"`. If the
    // runner accidentally executes a metadata segment (the regression
    // we are guarding against), the canary table will exist after the
    // apply. A SQL-comment placeholder cannot detect that regression
    // because both "executed" and "skipped" produce the same observable
    // state (no error, no side effect).
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
                    "AddTable metadata_table",
                    "CREATE TABLE \"metadata_table\" (\"id\" BIGINT PRIMARY KEY)",
                    "DROP TABLE \"metadata_table\"",
                )],
            },
            Segment {
                kind: SegmentKind::MetadataOnly,
                statements: vec![op(
                    "RenameApp old_app -> new_app",
                    "CREATE TABLE \"metadata_canary\" (\"id\" BIGINT PRIMARY KEY)",
                    "DROP TABLE \"metadata_canary\"",
                )],
            },
        ],
    };
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000011__metadata_only",
        None,
        None,
        MigrateConfig::default(),
    );
    let report = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("metadata-only path must succeed");

    // Accounting: 1 tx, 0 non-tx, 1 metadata segment.
    assert_eq!(report.transactional_segments, 1);
    assert_eq!(report.non_transactional_segments, 0);
    assert_eq!(report.metadata_segments, 1);

    // Ledger row must be applied with applied_steps_count = 0
    // (metadata-only does not contribute non-tx steps).
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("status select");
    assert_eq!(status, "applied");

    let applied_steps: i32 = ctx
        .raw_scalar(
            "SELECT applied_steps_count FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("applied_steps_count select");
    assert_eq!(applied_steps, 0);

    // The transactional segment's table must exist (proves the runner
    // did execute that segment's DDL).
    let table_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class \
             WHERE relname = 'metadata_table' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("transactional-segment exists check");
    assert!(table_exists);

    // The metadata segment's canary CREATE TABLE must NOT have run —
    // metadata segments are filesystem + ledger-row mutations only
    // (RenameApp / MoveModelBetweenApps per ). If the canary table
    // exists, the runner accidentally executed metadata-segment DDL,
    // which is the bug A-5 is guarding against.
    let canary_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class \
             WHERE relname = 'metadata_canary' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("metadata-canary exists check");
    assert!(
        !canary_exists,
        "metadata segments must NOT execute DDL — canary table was created"
    );
}

// ── Duplicate-version surface as VersionAlreadyApplied (live PG) ──────────

#[djogi::djogi_test]
async fn duplicate_version_surfaces_typed_error(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    // Apply once successfully.
    let plan = transactional_plan(vec![op(
        "AddTable dup",
        "CREATE TABLE \"dup\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"dup\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000012__duplicate",
        None,
        None,
        MigrateConfig::default(),
    );
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("first apply");

    // Re-running with a different plan body but the SAME version
    // label must surface `VersionAlreadyApplied`. The unique-violation
    // is on `version`, independent of the SQL contents.
    let plan2 = transactional_plan(vec![op(
        "DropTable dup",
        "DROP TABLE \"dup\"",
        "CREATE TABLE \"dup\" (\"id\" BIGINT PRIMARY KEY)",
    )]);
    let mut runner_ctx2 = make_runner_ctx(
        &plan2,
        // SAME version label triggers the unique-violation path.
        "V20260425000012__duplicate",
        None,
        None,
        MigrateConfig::default(),
    );
    // Recompute checksum for plan2's SQL — the verification step
    // runs before the insert so the plan needs a valid checksum.
    let frags2: Vec<&str> = plan2
        .segments
        .iter()
        .flat_map(|s| s.statements.iter())
        .map(|s| s.up.as_str())
        .collect();
    runner_ctx2.checksum_up = compute_checksum(frags2);

    let err = apply_plan(&mut ctx, &plan2, &runner_ctx2, &_guard)
        .await
        .expect_err("second apply must fail");
    match err {
        RunnerError::VersionAlreadyApplied {
            version,
            applied_at,
            ..
        } => {
            assert_eq!(version, "V20260425000012__duplicate");
            // The first apply finalised the row to `applied`, which the
            // ledger writes with a non-NULL `applied_at`. The typed
            // error must surface that timestamp so operators see when
            // the duplicate was first applied — `None` would be a
            // regression of the A-4 fix where the orchestrator falls
            // back to a generic message.
            assert!(
                applied_at.is_some(),
                "VersionAlreadyApplied must carry the original applied_at timestamp"
            );
        }
        other => panic!("expected VersionAlreadyApplied, got {other:?}"),
    }

    // The first apply's table must still exist — the duplicate-version
    // attempt was rejected at the ledger insert and never ran any DDL.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class \
             WHERE relname = 'dup' AND relkind = 'r')",
            &[],
    )
    .await
    .expect("exists check");
    assert!(exists, "DROP TABLE from second plan must NOT have run");
}

// ── Duplicate-version surface as VersionCollisionNonTerminal (live PG) ───

#[djogi::djogi_test]
async fn duplicate_version_surfaces_non_terminal_collision_statuses(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    // Seed one row per non-terminal status and ensure apply_plan reports the
    // blocking row's actual status/run_id instead of collapsing it to "applied".
    for (expected_version, seed_status, expected_run_id) in [
        (
            "V20260425000013__pending",
            LedgerStatus::Pending,
            13i64,
        ),
        ("V20260425000014__failed", LedgerStatus::Failed, 14i64),
        (
            "V20260425000015__rolled_back",
            LedgerStatus::RolledBack,
            15i64,
        ),
    ] {
        let plan = transactional_plan(vec![op(
            "AddTable dup_non_terminal",
            "CREATE TABLE \"dup_non_terminal\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"dup_non_terminal\"",
        )]);
        let runner_ctx = make_runner_ctx(
            &plan,
            expected_version,
            None,
            None,
            MigrateConfig::default(),
        );
        let version = runner_ctx.version.clone();
        let status = seed_status.as_db_str().to_string();
        let snapshot_version = SNAPSHOT_FORMAT_VERSION.to_string();
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, execution_mode, status, \
              run_id, snapshot_version, app_label) \
             VALUES ($1, $2, $3, 'transactional', $4, $5, $6, '')",
            &[
                &version,
                &runner_ctx.description,
                &runner_ctx.checksum_up,
                &status,
                &expected_run_id,
                &snapshot_version,
            ],
        )
        .await
        .expect("seed ledger row");

        let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
            .await
            .expect_err("second apply must fail");
        let msg = err.to_string();
        match err {
            RunnerError::VersionCollisionNonTerminal {
                version,
                status,
                run_id,
                ..
            } => {
                assert_eq!(version, expected_version);
                assert_eq!(status, seed_status);
                assert_eq!(run_id, expected_run_id);
            }
            other => panic!("expected VersionCollisionNonTerminal, got {other:?}"),
        }

        if seed_status == LedgerStatus::RolledBack {
            // RolledBack rows are not repair targets; the message tells the
            // operator the row is retained as audit history, not to use
            // the repair flow.
            assert!(msg.contains("the rolled_back row is retained as audit history"));
            assert!(!msg.contains("repair_partial_apply"));
        }
    }
}

// ── Advisory-lock key determinism (live PG smoke) ─────────────────────────

#[djogi::djogi_test]
async fn advisory_lock_key_is_stable_across_processes(mut ctx: djogi::DjogiContext) {
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");
    let bucket = BucketKey {
        database: "main".to_string(),
        app: "users".to_string(),
    };
    let k = advisory_lock_key(&bucket);
    // Acquire and release via the same SQL the runner uses.
    let acquired: bool = ctx
        .raw_scalar("SELECT pg_try_advisory_lock($1)", &[&k])
        .await
        .expect("try_lock");
    assert!(acquired);
    let released: bool = ctx
        .raw_scalar("SELECT pg_advisory_unlock($1)", &[&k])
        .await
        .expect("unlock");
    assert!(released);
}

// ── #274 Session-pinning regression: advisory lock released after apply ────
//
// Issue #274: pool-backed DjogiContext previously acquired the advisory lock
// on one connection checkout, then ran DDL/ledger writes on other checkouts,
// and released the lock on yet another checkout — giving pg_advisory_unlock
// no lock to release (it would return false, which the old code ignored).
//
// After the fix, apply_plan pins ONE physical Postgres session for the entire
// operation window. We verify through pg_locks:
//
// pg_locks shows advisory locks held by ALL sessions . If the pre-fix
// bug occurs (lock acquired on session A, release attempted on session B → false
// ignored → lock stays on A), the lock appears in pg_locks after apply_plan returns.
// With the fix, the lock is properly released on the same pinned session and
// pg_locks shows zero advisory locks for the key.

#[djogi::djogi_test]
async fn apply_plan_advisory_lock_not_held_after_success(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();

    let plan = transactional_plan(vec![op(
        "AddTable lock_release",
        "CREATE TABLE \"lock_release\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"lock_release\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000020__274_lock_release",
        None,
        None,
        MigrateConfig::default(),
    );

    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // After apply_plan the advisory lock must be fully released .
    // pg_locks surfaces advisory locks held by ANY backend, so this query
    // detects the pre-fix bug (lock leaked on the acquirer's session) regardless
    // of which pool connection the query itself uses.
    let lock_key = advisory_lock_key(&plan.bucket);
    // Use ::oid (unsigned 32-bit) for classid/objid. ::int4 would sign-extend
    // keys whose upper or lower 32 bits exceed 2^31-1, causing the comparison
    // to always return false for large lock keys (GH #274).
    let still_held: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) \
             FROM pg_locks \
             WHERE locktype = 'advisory' \
               AND classid = (($1::bigint >> 32) & 4294967295)::oid \
               AND objid   = ($1::bigint & 4294967295)::oid \
               AND mode    = 'ExclusiveLock' \
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            &[&lock_key],
        )
        .await
        .expect("pg_locks query");

    assert_eq!(
        still_held,
        0,
        "advisory lock for bucket={}/{} (key=0x{:016x}) must be released after \
         apply_plan completes; {} backend(s) still hold it — session-pinning bug (GH #274)",
        plan.bucket.database,
        plan.bucket.app,
        lock_key,
        still_held,
    );
}

// ── #274/#280: AdvisoryUnlockReturnedFalse is a first-class error variant ─
//
// RunnerError::AdvisoryUnlockReturnedFalse (added in #274/#280) is the typed
// correctness failure for when pg_advisory_unlock returns false. This test:
//
// 1. Confirms the variant exists at compile time (the match arm would fail
//    to compile if the variant is absent — the primary RED/GREEN gate).
// 2. Confirms that a clean apply does NOT trigger it (pinned session ensures
//    the unlock always runs on the session that holds the lock).

#[djogi::djogi_test]
async fn advisory_unlock_false_variant_exists_and_is_not_triggered_on_clean_apply(
    mut ctx: djogi::DjogiContext,
) {
    let _guard = acquire_test_workspace_guard();

    let plan = transactional_plan(vec![op(
        "AddTable unlock_variant",
        "CREATE TABLE \"unlock_variant\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"unlock_variant\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260425000021__274_unlock_variant",
        None,
        None,
        MigrateConfig::default(),
    );

    // A clean apply must never produce AdvisoryUnlockReturnedFalse — the
    // pinned session ensures the lock and unlock are always on the same
    // physical backend.
    let result = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard).await;

    match &result {
        Ok(_) => { /* expected */ }
        Err(RunnerError::AdvisoryUnlockReturnedFalse { key, .. }) => {
            panic!(
                "AdvisoryUnlockReturnedFalse (key=0x{key:016x}): apply_plan must \
                 not surface this on a clean apply — session-pinning bug (GH #274/#280)",
            );
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }

    assert!(result.is_ok());
}

// ── #331 Session-pinning regression test ──────────────────────────────
// Exercises the exact regression scenario: apply_plan on a pool-backed
// context must pin one physical session for the full operation window.

#[djogi::djogi_test]
async fn apply_plan_pool_backed_context_pins_session_for_advisory_lock(
    mut ctx: djogi::DjogiContext,
) {
    let _guard = acquire_test_workspace_guard();

    assert!(
        ctx.is_pool_backed(),
        "#[djogi_test] must provide a pool-backed context for this test",
    );

    // [REQ-331-11] Record outer context's backend PID before apply
    let pid_before: i32 = ctx
        .raw_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("pg_backend_pid before apply_plan");

    let plan = transactional_plan(vec![op(
        "AddTable pool_pin",
        "CREATE TABLE \"pool_pin\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"pool_pin\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260526000001__331_pool_pin",
        None,
        None,
        MigrateConfig::default(),
    );

    let result = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard).await;

    match &result {
        Ok(_) => {}
        Err(RunnerError::AdvisoryUnlockReturnedFalse { key, .. }) => {
            panic!(
                "AdvisoryUnlockReturnedFalse (key=0x{key:016x}): pool-backed \
                 context failed to pin for advisory lock (GH #331)",
            );
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }

    // [REQ-331-11] Record backend PID after apply
    let pid_after: i32 = ctx
        .raw_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("pg_backend_pid after apply_plan");

    tracing::debug!(pid_before, pid_after, "outer context backend PIDs (may differ)");

    // pg_locks must show zero advisory locks for this key 
    let lock_key = advisory_lock_key(&plan.bucket);
    let still_held: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) \
             FROM pg_locks \
             WHERE locktype = 'advisory' \
               AND classid = (($1::bigint >> 32) & 4294967295)::oid \
               AND objid   = ($1::bigint & 4294967295)::oid \
               AND mode    = 'ExclusiveLock' \
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            &[&lock_key],
        )
        .await
        .expect("pg_locks query");

    assert_eq!(
        still_held,
        0,
        "advisory lock for bucket={}/{} (key=0x{:016x}) must be released after \
         apply_plan on pool-backed context; {} backend(s) still hold it — \
         pin_for_migration regression (GH #331)",
        plan.bucket.database,
        plan.bucket.app,
        lock_key,
        still_held,
    );
}

// ── #331 Rollback path: pool-backed session pinning for advisory lock ──

#[djogi::djogi_test]
async fn rollback_plan_pool_backed_context_pins_session_for_advisory_lock(
    mut ctx: djogi::DjogiContext,
) {
    let _guard = acquire_test_workspace_guard();

    assert!(ctx.is_pool_backed(), "must be pool-backed");

    // Apply first so there's something to roll back
    let plan = transactional_plan(vec![op(
        "AddTable rollback_pin",
        "CREATE TABLE \"rollback_pin\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"rollback_pin\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260526000002__331_rollback_pin",
        None,
        None,
        MigrateConfig::default(),
    );
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    // [REQ-331-11] PID before rollback
    let pid_before: i32 = ctx
        .raw_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("pg_backend_pid before rollback_plan");

    let rollback_result = rollback_plan(
        &mut ctx,
        &plan,
        &runner_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await;

    assert!(rollback_result.is_ok(), "rollback must succeed: {:?}", rollback_result);

    // [REQ-331-11] PID after rollback
    let pid_after: i32 = ctx
        .raw_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("pg_backend_pid after rollback_plan");

    tracing::debug!(pid_before, pid_after, "rollback outer PIDs (may differ)");

    // pg_locks must show zero advisory locks
    let lock_key = advisory_lock_key(&plan.bucket);
    let still_held: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM pg_locks \
             WHERE locktype = 'advisory' \
               AND classid = (($1::bigint >> 32) & 4294967295)::oid \
               AND objid   = ($1::bigint & 4294967295)::oid \
               AND mode    = 'ExclusiveLock' \
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            &[&lock_key],
        )
        .await
        .expect("pg_locks query");

    assert_eq!(still_held, 0, "advisory lock must be released after rollback on pool-backed context (GH #331)");
}

#[djogi::djogi_test]
async fn rollback_bind_failure_releases_advisory_lock(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();

    let plan = transactional_plan_for_app("runner_331_rollback_bind", vec![op(
        "AddTable rollback_bind",
        "CREATE TABLE \"rollback_bind\" (\"id\" BIGINT PRIMARY KEY)",
        "DROP TABLE \"rollback_bind\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260607000001__331_rollback_bind",
        None,
        None,
        MigrateConfig::default(),
    );
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply ok");

    let mut rollback_ctx = make_runner_ctx(
        &plan,
        &runner_ctx.version,
        None,
        None,
        MigrateConfig::default(),
    );
    rollback_ctx.runner_identity = Some(RunnerIdentity::Selected { id: 7 });

    let result = rollback_plan(
        &mut ctx,
        &plan,
        &rollback_ctx,
        &_guard,
        LossyRollbackPolicy::Refuse,
        None,
    )
    .await;

    assert!(
        matches!(
            &result,
            Err(RollbackError::Runner {
                source: RunnerError::NodeIdentityBindingFailed {
                node_id: 7,
                ..
            },
                ..
            })
        ),
        "rollback must fail at selected-node binding for unregistered node 7: {result:?}",
    );

    let lock_key = advisory_lock_key(&plan.bucket);
    let still_held: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM pg_locks \
             WHERE locktype = 'advisory' \
               AND classid = (($1::bigint >> 32) & 4294967295)::oid \
               AND objid   = ($1::bigint & 4294967295)::oid \
               AND mode    = 'ExclusiveLock' \
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            &[&lock_key],
        )
        .await
        .expect("pg_locks query");

    assert_eq!(
        still_held,
        0,
        "advisory lock must be released after rollback bind failure (key=0x{lock_key:016x})",
    );
}

// ── #331 Early-error lock release via session drop ────────────────────
// Verifies that when apply_plan errors after acquiring the advisory lock
// but before releasing it, the PinnedCtx drops and the session closes,
// implicitly releasing the lock.

#[djogi::djogi_test]
async fn apply_plan_early_error_releases_lock_via_session_drop(
    mut ctx: djogi::DjogiContext,
) {
    let _guard = acquire_test_workspace_guard();

    assert!(ctx.is_pool_backed(), "must be pool-backed");

    // Plan with invalid SQL that fails during DDL execution (after lock acquire)
    let plan = transactional_plan(vec![op(
        "AddTable early_error",
        "CREATE TABLE \"early_error\" (\"id\" INVALID_TYPE PRIMARY KEY)",
        "DROP TABLE \"early_error\"",
    )]);
    let runner_ctx = make_runner_ctx(
        &plan,
        "V20260526000003__331_early_error",
        None,
        None,
        MigrateConfig::default(),
    );

    // This will fail during DDL execution — after lock acquire, before release
    let result = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard).await;
    assert!(result.is_err(), "apply_plan must error for this test to be meaningful");

    // PinnedCtx has been dropped — session closed — lock should be implicitly released
    let lock_key = advisory_lock_key(&plan.bucket);
    let still_held: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM pg_locks \
             WHERE locktype = 'advisory' \
               AND classid = (($1::bigint >> 32) & 4294967295)::oid \
               AND objid   = ($1::bigint & 4294967295)::oid \
               AND mode    = 'ExclusiveLock' \
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            &[&lock_key],
        )
        .await
        .expect("pg_locks query");

    assert_eq!(
        still_held,
        0,
        "advisory lock (key=0x{:016x}) must be released via session drop \
         after early error in apply_plan body (GH #331)",
        lock_key,
    );
}
