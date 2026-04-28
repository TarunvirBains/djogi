//! Phase 7 T7 — live-PG integration tests for the out-of-order policy
//! gate, multi-DB guardrails, and the `attune` reconciliation
//! command.
//!
//! # What these tests prove
//!
//! Out-of-order policy:
//! - `OutOfOrderPolicy::AllowWithDiagnostic` records the row with
//!   `out_of_order_flag = TRUE` and writes a partial_apply_note
//!   describing the conflicting peer.
//! - `OutOfOrderPolicy::Reject` refuses the apply BEFORE inserting
//!   the pending ledger row, so a rejected attempt leaves no trace.
//! - `OutOfOrderPolicy::AllowExplicit { override_reason }` records
//!   the row + sets the flag + persists the operator-supplied
//!   rationale verbatim.
//! - Lexical version comparison: applying `V20260201` after
//!   `V20260301` triggers OOO; the reverse order does not.
//!
//! Multi-DB guardrails:
//! - `advisory_lock_key` is per-bucket — two distinct buckets do NOT
//!   contend on the lock.
//! - The same bucket DOES contend (a held lock blocks a second
//!   acquire).
//! - The lock-key namespace string is `djogi:advisory_lock:` so the
//!   key derivation is reproducible across implementations.
//!
//! `verify` D622 surfacing:
//! - A row with `out_of_order_flag = TRUE` produces a D622 warning by
//!   default; `policy.strict_out_of_order = true` upgrades to error.
//!
//! `attune`:
//! - DiffOnly mode is read-only — no ledger / disk mutation.
//! - Record mode inserts an `applied` row for an unrecorded SQL file
//!   without running the SQL.
//! - Squash mode refuses on a non-localhost connection.
//! - Squash mode refuses on production profile.
//! - Squash mode refuses without `--from <ver>`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use djogi::config::MigrateConfig;
use djogi::migrate::{
    AppliedSchema, AttuneEntryKind, AttuneError, AttuneMode, AttuneRefusal, AttuneRequest,
    BucketKey, Classification, MigrationPlan, OperationSql, OutOfOrderPolicy, RunnerCtx,
    RunnerError, SNAPSHOT_FORMAT_VERSION, Segment, SegmentKind, VerifySeverity, WorkspaceGuard,
    acquire_workspace_lock, advisory_lock_key, apply_plan, attune, compute_checksum,
    is_localhost_connection, verify_with_policy,
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

fn temp_lock_path(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("djogi-t7-{tag}-{stamp}.lock"))
}

fn temp_workspace(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("djogi-t7-{tag}-{stamp}-{n}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn acquire_test_workspace_guard(tag: &str) -> WorkspaceGuard {
    acquire_workspace_lock(&temp_lock_path(tag), Duration::from_secs(2))
        .expect("acquire workspace lock")
}

/// Resolve the connected test context's `current_database()` so the
/// on-disk fixture tree matches the active database (B-2 — attune
/// filters the disk scan to `current_database()`).
async fn current_database(ctx: &mut djogi::DjogiContext) -> String {
    ctx.raw_scalar::<String>("SELECT current_database()::text", &[])
        .await
        .expect("current_database")
}

fn op(label: &str, up: &str, down: &str) -> OperationSql {
    OperationSql {
        label: label.to_string(),
        up: up.to_string(),
        down: down.to_string(),
        lossy: None,
    }
}

fn transactional_plan(bucket_app: &str, stmts: Vec<OperationSql>) -> MigrationPlan {
    MigrationPlan {
        bucket: BucketKey {
            database: "main".to_string(),
            app: bucket_app.to_string(),
        },
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: stmts,
        }],
    }
}

fn make_runner_ctx(plan: &MigrationPlan, version: &str, policy: OutOfOrderPolicy) -> RunnerCtx {
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
        description: format!("test {version}"),
        checksum_up,
        checksum_down: None,
        snapshot: None,
        snapshot_path: None,
        config: MigrateConfig::default(),
        out_of_order_policy: policy,
    }
}

// ── Out-of-order policy: AllowWithDiagnostic ──────────────────────────────

#[djogi::djogi_test]
async fn ooo_allow_with_diagnostic_records_flag_and_note(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard("ooo_allow");
    // Apply a "later" version first.
    let plan_late = transactional_plan(
        "",
        vec![op(
            "AddTable t7_late",
            "CREATE TABLE \"t7_ooo_allow_late\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_ooo_allow_late\"",
        )],
    );
    let runner_ctx_late = make_runner_ctx(
        &plan_late,
        "V20260301000000__late",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_late, &runner_ctx_late, &_guard)
        .await
        .expect("late ok");

    // Now apply an "earlier" version under AllowWithDiagnostic.
    let plan_early = transactional_plan(
        "",
        vec![op(
            "AddTable t7_early",
            "CREATE TABLE \"t7_ooo_allow_early\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_ooo_allow_early\"",
        )],
    );
    let runner_ctx_early = make_runner_ctx(
        &plan_early,
        "V20260201000000__early",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_early, &runner_ctx_early, &_guard)
        .await
        .expect("early apply must succeed under AllowWithDiagnostic");

    // out_of_order_flag must be true on the early row.
    let flag: bool = ctx
        .raw_scalar(
            "SELECT out_of_order_flag FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx_early.version],
        )
        .await
        .expect("flag select");
    assert!(flag, "early row must carry out_of_order_flag = TRUE");

    // partial_apply_note must mention the conflicting peer.
    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx_early.version],
        )
        .await
        .expect("note select");
    let note = note.expect("partial_apply_note must be set");
    assert!(
        note.contains("V20260301000000__late"),
        "note must mention the peer: {note}"
    );

    // Both tables must exist (apply happened end-to-end).
    let tables_exist: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = 't7_ooo_allow_early' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("table check");
    assert!(tables_exist);
}

// ── Out-of-order policy: Reject ───────────────────────────────────────────

#[djogi::djogi_test]
async fn ooo_reject_refuses_before_ledger_insert(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard("ooo_reject");
    // Apply a "later" version first.
    let plan_late = transactional_plan(
        "",
        vec![op(
            "AddTable t7_reject_late",
            "CREATE TABLE \"t7_ooo_reject_late\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_ooo_reject_late\"",
        )],
    );
    let runner_ctx_late = make_runner_ctx(
        &plan_late,
        "V20260301000000__late",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_late, &runner_ctx_late, &_guard)
        .await
        .expect("late ok");

    // Now attempt earlier under Reject — must error.
    let plan_early = transactional_plan(
        "",
        vec![op(
            "AddTable t7_reject_early",
            "CREATE TABLE \"t7_ooo_reject_early\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_ooo_reject_early\"",
        )],
    );
    let runner_ctx_early = make_runner_ctx(
        &plan_early,
        "V20260201000000__early",
        OutOfOrderPolicy::Reject,
    );
    let err = apply_plan(&mut ctx, &plan_early, &runner_ctx_early, &_guard)
        .await
        .expect_err("Reject policy must fail the apply");
    match err {
        RunnerError::OutOfOrderRejected {
            version,
            conflicting_version,
            conflicting_applied_at,
        } => {
            assert_eq!(version, "V20260201000000__early");
            assert_eq!(conflicting_version, "V20260301000000__late");
            assert!(
                conflicting_applied_at.is_some(),
                "applied_at must be populated"
            );
        }
        other => panic!("expected OutOfOrderRejected, got {other:?}"),
    }

    // Critically: NO ledger row was inserted for the rejected version.
    let count: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx_early.version],
        )
        .await
        .expect("count");
    assert_eq!(count, 0, "rejected apply must NOT insert a ledger row");

    // The early table must NOT have been created either.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = 't7_ooo_reject_early' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(!exists, "rejected apply must not create user tables");
}

// ── Out-of-order policy: AllowExplicit ────────────────────────────────────

#[djogi::djogi_test]
async fn ooo_allow_explicit_records_override_reason(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard("ooo_explicit");
    let plan_late = transactional_plan(
        "",
        vec![op(
            "AddTable t7_exp_late",
            "CREATE TABLE \"t7_ooo_exp_late\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_ooo_exp_late\"",
        )],
    );
    let runner_ctx_late = make_runner_ctx(
        &plan_late,
        "V20260301000000__exp_late",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_late, &runner_ctx_late, &_guard)
        .await
        .expect("late ok");

    let plan_early = transactional_plan(
        "",
        vec![op(
            "AddTable t7_exp_early",
            "CREATE TABLE \"t7_ooo_exp_early\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_ooo_exp_early\"",
        )],
    );
    let override_reason = "cherry-picking from main into the dev branch".to_string();
    let runner_ctx_early = make_runner_ctx(
        &plan_early,
        "V20260201000000__exp_early",
        OutOfOrderPolicy::AllowExplicit {
            override_reason: override_reason.clone(),
        },
    );
    apply_plan(&mut ctx, &plan_early, &runner_ctx_early, &_guard)
        .await
        .expect("AllowExplicit must permit the apply");

    let flag: bool = ctx
        .raw_scalar(
            "SELECT out_of_order_flag FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx_early.version],
        )
        .await
        .expect("flag");
    assert!(flag);

    let note: Option<String> = ctx
        .raw_scalar(
            "SELECT partial_apply_note FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx_early.version],
        )
        .await
        .expect("note");
    let note = note.expect("partial_apply_note must be set");
    assert!(
        note.contains(&override_reason),
        "override reason must be persisted verbatim: {note}"
    );
    assert!(note.contains("override"), "note: {note}");
}

// ── In-order apply: no out-of-order flag ──────────────────────────────────

#[djogi::djogi_test]
async fn in_order_apply_does_not_set_ooo_flag(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard("in_order");
    let plan_a = transactional_plan(
        "",
        vec![op(
            "AddTable t7_io_a",
            "CREATE TABLE \"t7_io_a\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_io_a\"",
        )],
    );
    let runner_ctx_a = make_runner_ctx(
        &plan_a,
        "V20260101000000__a",
        OutOfOrderPolicy::Reject, // even Reject must allow strictly in-order
    );
    apply_plan(&mut ctx, &plan_a, &runner_ctx_a, &_guard)
        .await
        .expect("first apply ok");

    let plan_b = transactional_plan(
        "",
        vec![op(
            "AddTable t7_io_b",
            "CREATE TABLE \"t7_io_b\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_io_b\"",
        )],
    );
    let runner_ctx_b = make_runner_ctx(&plan_b, "V20260201000000__b", OutOfOrderPolicy::Reject);
    apply_plan(&mut ctx, &plan_b, &runner_ctx_b, &_guard)
        .await
        .expect("in-order second apply must succeed even under Reject");

    let flag: bool = ctx
        .raw_scalar(
            "SELECT out_of_order_flag FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx_b.version],
        )
        .await
        .expect("flag");
    assert!(!flag, "in-order apply must NOT set out_of_order_flag");
}

// ── Multi-DB advisory lock independence ───────────────────────────────────

#[djogi::djogi_test]
async fn advisory_lock_key_differs_across_databases(_ctx: djogi::DjogiContext) {
    let bucket_a = BucketKey {
        database: "main".to_string(),
        app: "billing".to_string(),
    };
    let bucket_b = BucketKey {
        database: "crud_log".to_string(),
        app: "billing".to_string(),
    };
    let key_a = advisory_lock_key(&bucket_a);
    let key_b = advisory_lock_key(&bucket_b);
    assert_ne!(
        key_a, key_b,
        "different databases must derive different advisory-lock keys"
    );
}

// ── Multi-DB advisory lock: same bucket contends, different doesn't ───────

#[djogi::djogi_test]
async fn advisory_lock_same_bucket_blocks_different_does_not(mut ctx: djogi::DjogiContext) {
    // A-2: Postgres advisory locks are reentrant within a single
    // session (a second `pg_try_advisory_lock` from the same
    // connection always succeeds). To prove cross-session contention
    // we need TWO sessions. We open a second `DjogiPool` against the
    // SAME test database — `current_database()` from the existing ctx
    // gives us the database name, and we splice it into the harness's
    // `DATABASE_URL` to derive the second connection URL.
    let db = current_database(&mut ctx).await;
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    // Splice the test db name into the URL. The harness uses standard
    // `postgres://[user[:pass]@]host[:port][/db]` URLs; we replace the
    // path after the host's `/` (or append `/db` when no path is
    // present).
    let second_url = splice_db_into_url(&admin_url, &db);
    let second_pool = djogi::pg::pool::DjogiPool::connect(&second_url)
        .await
        .expect("second pool");
    let mut ctx_b = djogi::DjogiContext::from_pool(second_pool);

    let bucket_same = BucketKey {
        database: db.clone(),
        app: "billing".to_string(),
    };
    let bucket_other = BucketKey {
        database: db.clone(),
        app: "users".to_string(),
    };
    let key_same = advisory_lock_key(&bucket_same);
    let key_other = advisory_lock_key(&bucket_other);

    // Session 1 acquires the lock on `key_same`. Use a session-scoped
    // (non-transactional) advisory lock — `pg_try_advisory_lock` —
    // so it persists across statements.
    let acquired_session1: bool = ctx
        .raw_scalar("SELECT pg_try_advisory_lock($1)", &[&key_same])
        .await
        .expect("session 1 acquire");
    assert!(acquired_session1, "session 1 must acquire same-bucket lock");

    // Session 2 attempts the SAME key — must fail (cross-session
    // contention). This is the contract the runner depends on:
    // concurrent apply against the same bucket cannot interleave.
    let acquired_session2_same: bool = ctx_b
        .raw_scalar("SELECT pg_try_advisory_lock($1)", &[&key_same])
        .await
        .expect("session 2 try same");
    assert!(
        !acquired_session2_same,
        "session 2 must FAIL to acquire the same key already held by session 1 \
         (cross-session contention is the runner's safety contract)"
    );

    // Session 2 attempts a DIFFERENT bucket's key — must succeed.
    // This proves keys are scoped per-bucket: an apply against one
    // bucket never blocks an apply against a different bucket.
    let acquired_session2_other: bool = ctx_b
        .raw_scalar("SELECT pg_try_advisory_lock($1)", &[&key_other])
        .await
        .expect("session 2 try other");
    assert!(
        acquired_session2_other,
        "session 2 must acquire a different-bucket key independently of session 1"
    );

    // Session 1 releases. Session 2 retries the same key — must now
    // succeed.
    let released_session1: bool = ctx
        .raw_scalar("SELECT pg_advisory_unlock($1)", &[&key_same])
        .await
        .expect("session 1 release");
    assert!(released_session1, "session 1 unlock must report true");

    let acquired_session2_after_release: bool = ctx_b
        .raw_scalar("SELECT pg_try_advisory_lock($1)", &[&key_same])
        .await
        .expect("session 2 retry after release");
    assert!(
        acquired_session2_after_release,
        "session 2 must acquire the key after session 1 released it"
    );

    // Cleanup — release the locks session 2 holds.
    let _: bool = ctx_b
        .raw_scalar("SELECT pg_advisory_unlock($1)", &[&key_same])
        .await
        .expect("session 2 unlock same");
    let _: bool = ctx_b
        .raw_scalar("SELECT pg_advisory_unlock($1)", &[&key_other])
        .await
        .expect("session 2 unlock other");
}

/// Splice `new_db` into a libpq-style URL by replacing the path
/// component (or appending it when absent). Byte-level, no regex.
fn splice_db_into_url(url: &str, new_db: &str) -> String {
    // Strip the scheme prefix if present so we can locate the next
    // `/` boundary cleanly.
    let (scheme, rest) = if let Some(r) = url.strip_prefix("postgres://") {
        ("postgres://", r)
    } else if let Some(r) = url.strip_prefix("postgresql://") {
        ("postgresql://", r)
    } else {
        // Not a URL — return verbatim; harness always uses postgres:// form.
        return url.to_string();
    };
    // Find the first `/` in the body (separates authority from path).
    // The authority itself may contain `/` only inside an IPv6 bracketed
    // form, which doesn't apply here — the harness URL is a plain
    // host:port form.
    let bytes = rest.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && bytes[i] != b'/' && bytes[i] != b'?' {
        i += 1;
    }
    let authority = &rest[..i];
    let tail = &rest[i..];
    // Tail may be `/dbname?args`, `/dbname`, `?args`, or `""`. Replace
    // only the dbname segment.
    let query_start = tail.find('?').unwrap_or(tail.len());
    let after_query = &tail[query_start..];
    format!("{scheme}{authority}/{new_db}{after_query}")
}

// ── verify D622: out-of-order surfaced as warning by default ──────────────

#[djogi::djogi_test]
async fn verify_d622_warning_default_policy(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard("d622_warn");
    // Apply late then early to seed an OOO row.
    let plan_late = transactional_plan(
        "",
        vec![op(
            "AddTable t7_d622_late",
            "CREATE TABLE \"t7_d622_late\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_d622_late\"",
        )],
    );
    let ctx_late = make_runner_ctx(
        &plan_late,
        "V20260301000000__d622_late",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_late, &ctx_late, &_guard)
        .await
        .expect("late ok");
    let plan_early = transactional_plan(
        "",
        vec![op(
            "AddTable t7_d622_early",
            "CREATE TABLE \"t7_d622_early\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_d622_early\"",
        )],
    );
    let ctx_early = make_runner_ctx(
        &plan_early,
        "V20260201000000__d622_early",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_early, &ctx_early, &_guard)
        .await
        .expect("early ok");

    // Project a snapshot containing the two tables so the verify run
    // does not emit unrelated drift diagnostics.
    let snapshot = empty_snapshot();
    let policy = djogi::config::PolicyConfig::default();
    let report = verify_with_policy(&mut ctx, &snapshot, &policy)
        .await
        .expect("verify ok");
    let d622: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "D622")
        .collect();
    assert!(!d622.is_empty(), "D622 must be present");
    assert_eq!(d622[0].severity, VerifySeverity::Warning);
}

// ── verify D622 strict mode upgrades to Error ─────────────────────────────

#[djogi::djogi_test]
async fn verify_d622_strict_mode_is_error(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard("d622_strict");
    let plan_late = transactional_plan(
        "",
        vec![op(
            "AddTable t7_strict_late",
            "CREATE TABLE \"t7_d622_strict_late\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_d622_strict_late\"",
        )],
    );
    let ctx_late = make_runner_ctx(
        &plan_late,
        "V20260301000000__strict_late",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_late, &ctx_late, &_guard)
        .await
        .expect("late ok");
    let plan_early = transactional_plan(
        "",
        vec![op(
            "AddTable t7_strict_early",
            "CREATE TABLE \"t7_d622_strict_early\" (\"id\" BIGINT PRIMARY KEY)",
            "DROP TABLE \"t7_d622_strict_early\"",
        )],
    );
    let ctx_early = make_runner_ctx(
        &plan_early,
        "V20260201000000__strict_early",
        OutOfOrderPolicy::AllowWithDiagnostic,
    );
    apply_plan(&mut ctx, &plan_early, &ctx_early, &_guard)
        .await
        .expect("early ok");

    let snapshot = empty_snapshot();
    let policy = djogi::config::PolicyConfig {
        strict_out_of_order: true,
    };
    let report = verify_with_policy(&mut ctx, &snapshot, &policy)
        .await
        .expect("verify ok");
    let d622: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "D622")
        .collect();
    assert!(!d622.is_empty(), "D622 must be present");
    assert_eq!(d622[0].severity, VerifySeverity::Error);
    assert!(report.has_errors(), "strict mode must produce a hard error");
}

// ── attune DiffOnly: read-only ────────────────────────────────────────────

#[djogi::djogi_test]
async fn attune_diff_only_does_not_mutate(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("attune_diff");
    // Bootstrap the ledger ourselves so DiffOnly's read-only contract
    // passes — this test exercises diff semantics, not the
    // ledger-missing diagnostic surface (which has its own dedicated
    // test below).
    djogi::migrate::bootstrap_ledger(&mut ctx)
        .await
        .expect("bootstrap");
    // Lay down an unrecorded migration on disk under the active
    // database's bucket directory (B-2 — the disk scan is scoped to
    // `current_database()`).
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260101000001__init.sql"),
        "CREATE TABLE foo();",
    )
    .unwrap();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::DiffOnly,
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(!report.mutated, "DiffOnly must never mutate");
    // Must surface the unrecorded entry.
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.kind == AttuneEntryKind::Unrecorded && e.version == "V20260101000001__init"),
        "expected the unrecorded entry: {:?}",
        report.entries
    );

    // Confirm the ledger still has zero rows for that version.
    let count: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
            &[&"V20260101000001__init".to_string()],
        )
        .await
        .expect("count");
    assert_eq!(count, 0, "DiffOnly must not insert ledger rows");

    let _ = std::fs::remove_dir_all(&work);
}

// ── attune Record: inserts row, does NOT execute SQL ──────────────────────

#[djogi::djogi_test]
async fn attune_record_inserts_row_without_running_sql(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("attune_record");
    // Lay the fixture under the active database's bucket directory
    // (B-2 — the disk scan is scoped to `current_database()`).
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    // The SQL would create a table; if attune executes it, the table
    // would exist after.
    std::fs::write(
        bucket_dir.join("V20260101000002__record.sql"),
        "CREATE TABLE t7_attune_must_not_run_this_table(id INT);",
    )
    .unwrap();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Record {
            reason: "operator asserted".to_string(),
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(report.mutated, "Record must mutate ledger");
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.kind == AttuneEntryKind::Recorded && e.version == "V20260101000002__record"),
        "must report the Recorded entry: {:?}",
        report.entries
    );

    // Ledger row exists with status applied.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&"V20260101000002__record".to_string()],
        )
        .await
        .expect("status");
    assert_eq!(status, "applied");

    // CRITICAL: the SQL was NOT executed. The would-be table must NOT
    // exist.
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = 't7_attune_must_not_run_this_table' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists check");
    assert!(
        !exists,
        "Record mode must NOT execute SQL — the asserted-applied table appeared in the catalog"
    );

    let _ = std::fs::remove_dir_all(&work);
}

// ── attune Squash: refuses on remote DATABASE_URL ─────────────────────────

#[djogi::djogi_test]
async fn attune_squash_refuses_on_remote_db(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("attune_squash_remote");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://prod.example.com:5432/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashNotLocalhost { database_url }) => {
            assert_eq!(database_url, "postgres://prod.example.com:5432/main");
        }
        other => panic!("expected SquashNotLocalhost, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── attune Squash: refuses on production profile ──────────────────────────

#[djogi::djogi_test]
async fn attune_squash_refuses_on_production_profile(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("attune_squash_prod");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "production",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashNotDevProfile { profile }) => {
            assert_eq!(profile, "production");
        }
        other => panic!("expected SquashNotDevProfile, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── Codex umbrella U-2: --squash refuses when dev_mode is off ─────────────

/// Codex umbrella U-2 BLOCK: `attune --squash` must refuse when
/// `[database].dev_mode = false`. Pre-fix the field was documented in
/// `docs/spec/configuration.md` §14 but never read; an operator who
/// forgot to flip the flag still got the rewrite. This test exercises
/// the third gate — localhost + dev profile pass, but `dev_mode = false`
/// must surface `SquashDevModeOff` before any disk I/O.
#[djogi::djogi_test]
async fn u2_attune_squash_refuses_when_dev_mode_off(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u2_squash_dev_mode_off");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: false, // the gate under test
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashDevModeOff) => {}
        other => panic!("expected SquashDevModeOff, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── Codex umbrella U-2: --squash refuses when DJOGI_ENV=production ────────

/// Codex umbrella U-2 BLOCK: `attune --squash` must refuse when the
/// `DJOGI_ENV` environment variable is set to `"production"`
/// (case-insensitive ASCII compare). The env var is the
/// deployment-time signal that overrides the `Djogi.toml` profile —
/// CI / orchestration sets it before invoking `djogi`. This test
/// pins the env-var gate independently from the profile gate by
/// passing `profile = "development"` and `dev_mode = true`; the only
/// gate that should trip is the env-var one.
///
/// Tests run with `--test-threads=1` per the project's pre-commit
/// policy so concurrent env mutation is not a concern in this
/// configuration.
#[djogi::djogi_test]
async fn u2_attune_squash_refuses_when_djogi_env_is_production(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u2_squash_djogi_env_prod");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");

    // Save / restore DJOGI_ENV so the test does not leak state.
    let prior = std::env::var("DJOGI_ENV").ok();
    // SAFETY: serial test execution; no other thread reads DJOGI_ENV.
    unsafe { std::env::set_var("DJOGI_ENV", "production") };

    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashEnvIsProduction { env_value }) => {
            assert_eq!(env_value, "production");
        }
        other => panic!("expected SquashEnvIsProduction, got {other:?}"),
    }

    match prior {
        Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
        None => unsafe { std::env::remove_var("DJOGI_ENV") },
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── Codex umbrella U-2: gate ordering — localhost evaluated first ─────────

/// When MULTIPLE squash gates would refuse, the localhost gate must
/// fire FIRST. Operators get a single deterministic refusal reason
/// rather than a moving target. This pins the gate-1 → gate-4 order
/// documented in the `attune` module header so a future refactor that
/// reshuffles the gate evaluation surfaces immediately.
#[djogi::djogi_test]
async fn u2_attune_squash_gate_order_localhost_before_others(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u2_squash_gate_order");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        // All four gates would refuse this:
        database_url: "postgres://prod.example.com/main", // gate 1
        profile: "production",                            // gate 2
        dev_mode: false,                                  // gate 3
        target: None,
        apply: true,
        record: false,
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashNotLocalhost { .. }) => {}
        other => panic!("expected SquashNotLocalhost first, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── attune Squash: refuses on missing --from ──────────────────────────────

#[djogi::djogi_test]
async fn attune_squash_refuses_on_missing_from_version(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("attune_squash_no_from");
    // Lay one file but ask to squash from a non-existent starting
    // version.
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260201000000__seed.sql"),
        "CREATE TABLE foo();",
    )
    .unwrap();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__nonexistent".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        // After B-5, an absent version is reported via the more
        // specific `SquashFromVersionNotFound` variant.
        AttuneError::Refused(AttuneRefusal::SquashFromVersionNotFound { version }) => {
            assert_eq!(version, "V20260101000000__nonexistent");
        }
        other => panic!("expected SquashFromVersionNotFound, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── attune Squash: success path (no --publish) ────────────────────────────

#[djogi::djogi_test]
async fn attune_squash_collapses_local_files(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("attune_squash_ok");
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();

    // Three migrations on disk.
    std::fs::write(
        bucket_dir.join("V20260101000000__init.sql"),
        "CREATE TABLE foo();",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260101000000__init.down.sql"),
        "DROP TABLE foo;",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260201000000__add_bar.sql"),
        "ALTER TABLE foo ADD COLUMN bar TEXT;",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260201000000__add_bar.down.sql"),
        "ALTER TABLE foo DROP COLUMN bar;",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260301000000__add_baz.sql"),
        "ALTER TABLE foo ADD COLUMN baz TEXT;",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260301000000__add_baz.down.sql"),
        "ALTER TABLE foo DROP COLUMN baz;",
    )
    .unwrap();

    // Seed three ledger rows: the retained `from` row plus the two
    // later rows. The retained row's `checksum_up` / `description`
    // start out describing the PRE-squash file content; B-4 requires
    // squash to refresh both to match the post-squash file.
    djogi::migrate::bootstrap_ledger(&mut ctx)
        .await
        .expect("bootstrap");
    for v in [
        "V20260101000000__init",
        "V20260201000000__add_bar",
        "V20260301000000__add_baz",
    ] {
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, checksum_down, execution_mode, status, \
              run_id, snapshot_version, app_label) \
             VALUES ($1, $2, $3, NULL, 'transactional', 'applied', 0, '1.0', '')",
            &[
                &v.to_string(),
                &"original pre-squash description".to_string(),
                &"V1:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            ],
        )
        .await
        .expect("insert");
    }

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("squash ok");
    assert!(report.mutated);
    assert_eq!(
        report.squashed_to,
        Some("V20260101000000__init".to_string())
    );
    assert!(!report.published, "publish=false must not push");

    // The two later up files must be GONE.
    assert!(
        !bucket_dir.join("V20260201000000__add_bar.sql").exists(),
        "later up file must be deleted"
    );
    assert!(
        !bucket_dir.join("V20260301000000__add_baz.sql").exists(),
        "later up file must be deleted"
    );
    // Their down files too.
    assert!(
        !bucket_dir
            .join("V20260201000000__add_bar.down.sql")
            .exists()
    );
    // The squash target file must still exist and contain ALL the
    // collapsed up SQL.
    let squashed = std::fs::read_to_string(bucket_dir.join("V20260101000000__init.sql"))
        .expect("squashed up file");
    assert!(squashed.contains("CREATE TABLE foo()"));
    assert!(squashed.contains("ADD COLUMN bar"));
    assert!(squashed.contains("ADD COLUMN baz"));

    // The ledger rows for the two later versions must be gone.
    for v in ["V20260201000000__add_bar", "V20260301000000__add_baz"] {
        let count: i64 = ctx
            .raw_scalar(
                "SELECT COUNT(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
                &[&v.to_string()],
            )
            .await
            .expect("count");
        assert_eq!(count, 0, "ledger row for {v} must be deleted");
    }

    // B-4: the retained `from` row's `checksum_up` and `description`
    // must describe the POST-squash file content, not the pre-squash
    // content. Recompute the checksum against the freshly-written
    // squashed file's bytes and confirm parity.
    let post_up_sql = std::fs::read_to_string(bucket_dir.join("V20260101000000__init.sql"))
        .expect("squashed up file (B-4 read)");
    let expected_checksum = djogi::migrate::compute_checksum([post_up_sql.as_str()]);
    let actual_checksum: String = ctx
        .raw_scalar(
            "SELECT checksum_up FROM djogi_schema_migrations WHERE version = $1",
            &[&"V20260101000000__init".to_string()],
        )
        .await
        .expect("checksum_up after squash");
    assert_eq!(
        actual_checksum, expected_checksum,
        "retained `from` row's checksum_up must match the post-squash file"
    );
    let actual_desc: String = ctx
        .raw_scalar(
            "SELECT description FROM djogi_schema_migrations WHERE version = $1",
            &[&"V20260101000000__init".to_string()],
        )
        .await
        .expect("description after squash");
    assert!(
        actual_desc.contains("squashed"),
        "retained row's description must mention `squashed`: {actual_desc}"
    );

    let _ = std::fs::remove_dir_all(&work);
}

// ── B-1 regression: padded `=` in libpq form refuses squash ──────────────

#[djogi::djogi_test]
async fn attune_squash_refuses_remote_via_padded_equals(mut ctx: djogi::DjogiContext) {
    // Pre-fix, `host = prod.example.com` returned `""` from the
    // libpq parser and the localhost allowlist treated `""` as
    // localhost (Unix-socket convention) — so squash would have
    // PASSED its localhost gate against a remote production DB. The
    // post-fix parser handles whitespace around `=`, so the host
    // resolves correctly to `prod.example.com` and squash refuses.
    let work = temp_workspace("attune_squash_padded_remote");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "host = prod.example.com dbname=main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashNotLocalhost { database_url }) => {
            assert_eq!(database_url, "host = prod.example.com dbname=main");
        }
        other => panic!("expected SquashNotLocalhost, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── B-3 regression: DiffOnly does NOT bootstrap the ledger ────────────────

#[djogi::djogi_test]
async fn attune_diff_only_does_not_bootstrap_ledger(mut ctx: djogi::DjogiContext) {
    // Fresh test database — the ledger table does not exist yet. Run
    // attune in DiffOnly mode and confirm:
    //
    // 1. The report carries the structured `LedgerTableMissing`
    //    diagnostic.
    // 2. The ledger table is STILL absent from `pg_class` after
    //    attune returns. (Pre-B-3, attune called `ledger::bootstrap`
    //    unconditionally, which silently created the table on every
    //    DiffOnly run — breaking the read-only contract.)
    let work = temp_workspace("attune_diff_no_bootstrap");
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::DiffOnly,
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("DiffOnly ok");
    assert!(!report.mutated, "DiffOnly must never mutate");
    assert!(
        report.diagnostics.iter().any(|d| matches!(
            d,
            djogi::migrate::AttuneDiagnostic::LedgerTableMissing { .. }
        )),
        "DiffOnly on fresh DB must surface LedgerTableMissing: {:?}",
        report.diagnostics
    );
    let exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("ledger probe");
    assert!(
        !exists,
        "DiffOnly must NOT create djogi_schema_migrations on a fresh DB"
    );
    let _ = std::fs::remove_dir_all(&work);
}

// ── B-2 regression: disk scan filters by current_database() ──────────────

#[djogi::djogi_test]
async fn attune_record_filters_disk_scan_by_active_database(mut ctx: djogi::DjogiContext) {
    // Lay BOTH `migrations/<active_db>/users/...` and
    // `migrations/other_db/billing/...` on disk. Run attune --record
    // against the active context. Only the active database's bucket
    // entries should land in the ledger; the other database's files
    // are out of scope.
    let work = temp_workspace("attune_record_db_scope");
    let active_db = current_database(&mut ctx).await;

    let active_dir = work.join(format!("migrations/{active_db}/users"));
    std::fs::create_dir_all(&active_dir).unwrap();
    std::fs::write(
        active_dir.join("V20260101000001__active.sql"),
        "CREATE TABLE t7_b2_active(id INT);",
    )
    .unwrap();

    let other_dir = work.join("migrations/other_database_b2/billing");
    std::fs::create_dir_all(&other_dir).unwrap();
    std::fs::write(
        other_dir.join("V20260101000002__other.sql"),
        "CREATE TABLE t7_b2_other(id INT);",
    )
    .unwrap();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Record {
            reason: "B-2 regression coverage".to_string(),
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(report.mutated);
    // The active-db version must be present.
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.version == "V20260101000001__active" && e.kind == AttuneEntryKind::Recorded),
        "active-db entry must be recorded: {:?}",
        report.entries
    );
    // The other-db version must NOT be present in the report at all.
    assert!(
        !report
            .entries
            .iter()
            .any(|e| e.version == "V20260101000002__other"),
        "other-db files must not appear in the active-db scan: {:?}",
        report.entries
    );
    // And no ledger row was inserted for the other-db version.
    let count: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
            &[&"V20260101000002__other".to_string()],
        )
        .await
        .expect("count");
    assert_eq!(
        count, 0,
        "ledger must NOT carry rows for files that belong to a different database"
    );
    let _ = std::fs::remove_dir_all(&work);
}

// ── B-5 regression: squash refuses when --from spans multiple buckets ────

#[djogi::djogi_test]
async fn attune_squash_refuses_ambiguous_from_across_buckets(mut ctx: djogi::DjogiContext) {
    // Two buckets in the active database both contain the same
    // version. Squash without `--app` must refuse with
    // `SquashFromVersionAmbiguous` rather than collapsing both
    // buckets' histories.
    let work = temp_workspace("attune_squash_ambiguous");
    let active_db = current_database(&mut ctx).await;

    let users_dir = work.join(format!("migrations/{active_db}/users"));
    let billing_dir = work.join(format!("migrations/{active_db}/billing"));
    std::fs::create_dir_all(&users_dir).unwrap();
    std::fs::create_dir_all(&billing_dir).unwrap();

    // Same version in BOTH buckets (a path that historically tripped
    // the pre-B-5 implementation into squashing both).
    let shared_version = "V20260101000000__shared";
    for dir in [&users_dir, &billing_dir] {
        std::fs::write(dir.join(format!("{shared_version}.sql")), "-- shared seed").unwrap();
        std::fs::write(dir.join("V20260601000000__later.sql"), "-- later").unwrap();
    }

    djogi::migrate::bootstrap_ledger(&mut ctx)
        .await
        .expect("bootstrap");

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: shared_version.to_string(),
            publish: false,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must refuse");
    match err {
        AttuneError::Refused(AttuneRefusal::SquashFromVersionAmbiguous { version, buckets }) => {
            assert_eq!(version, shared_version);
            assert_eq!(buckets.len(), 2);
            // Buckets are rendered as `database/app` strings.
            assert!(buckets.iter().any(|b| b.ends_with("/users")));
            assert!(buckets.iter().any(|b| b.ends_with("/billing")));
        }
        other => panic!("expected SquashFromVersionAmbiguous, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ── B-5: explicit --app scopes the squash to a single bucket ─────────────

#[djogi::djogi_test]
async fn attune_squash_with_app_filter_only_collapses_target_bucket(mut ctx: djogi::DjogiContext) {
    // Two buckets each carry an independent history. Squash with
    // `--app=users --from=<users-only-version>` must rewrite ONLY
    // the users bucket; billing must be untouched.
    let work = temp_workspace("attune_squash_app_filter");
    let active_db = current_database(&mut ctx).await;

    let users_dir = work.join(format!("migrations/{active_db}/users"));
    let billing_dir = work.join(format!("migrations/{active_db}/billing"));
    std::fs::create_dir_all(&users_dir).unwrap();
    std::fs::create_dir_all(&billing_dir).unwrap();

    std::fs::write(
        users_dir.join("V20260101000000__users_init.sql"),
        "CREATE TABLE u_init();",
    )
    .unwrap();
    std::fs::write(
        users_dir.join("V20260201000000__users_later.sql"),
        "ALTER TABLE u_init ADD COLUMN x INT;",
    )
    .unwrap();
    std::fs::write(
        billing_dir.join("V20260301000000__billing_init.sql"),
        "CREATE TABLE b_init();",
    )
    .unwrap();
    std::fs::write(
        billing_dir.join("V20260401000000__billing_later.sql"),
        "ALTER TABLE b_init ADD COLUMN y INT;",
    )
    .unwrap();

    djogi::migrate::bootstrap_ledger(&mut ctx)
        .await
        .expect("bootstrap");

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__users_init".to_string(),
            publish: false,
            app: Some("users".to_string()),
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("squash ok");
    assert!(report.mutated);

    // Users later file collapsed.
    assert!(
        !users_dir.join("V20260201000000__users_later.sql").exists(),
        "users-later must be deleted"
    );
    // Billing files untouched.
    assert!(
        billing_dir
            .join("V20260301000000__billing_init.sql")
            .exists(),
        "billing-init must remain"
    );
    assert!(
        billing_dir
            .join("V20260401000000__billing_later.sql")
            .exists(),
        "billing-later must remain"
    );
    let _ = std::fs::remove_dir_all(&work);
}

// ── A-1: --publish path against a fixture remote ─────────────────────────

/// Helper — tests if `git` is available on PATH. Skips the live
/// publish test gracefully when CI lacks git.
fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[djogi::djogi_test]
async fn attune_squash_with_publish_pushes_to_remote_origin(mut ctx: djogi::DjogiContext) {
    // A-1: prove the `--publish` path actually shells out to
    // `git -C <migrations_root> push` and reaches a configured
    // `origin` remote. Strategy:
    //
    // 1. Initialise `migrations/` as a git working repo with a fresh
    //    bare remote as `origin`.
    // 2. Lay two SQL files + commit + push the initial tree to
    //    `origin/main`.
    // 3. Seed ledger rows + run `attune --squash` WITHOUT `--publish`
    //    so the rewrite happens locally; then stage + commit the
    //    rewrite so HEAD differs from `origin/main`.
    // 4. Run `attune --squash` WITH `--publish` against a no-op
    //    starting version (collapses nothing further) so the
    //    publisher's git-push exit path is exercised cleanly.
    // 5. Verify `origin`'s HEAD now matches the local HEAD.
    if !git_available() {
        eprintln!(
            "[t7] skipping attune_squash_with_publish_pushes_to_remote_origin: \
             git not on PATH"
        );
        return;
    }

    let work = temp_workspace("attune_squash_publish");
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();

    // Three SQL files so squash has work to do.
    std::fs::write(
        bucket_dir.join("V20260101000000__init.sql"),
        "CREATE TABLE foo();",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260201000000__add_bar.sql"),
        "ALTER TABLE foo ADD COLUMN bar TEXT;",
    )
    .unwrap();
    std::fs::write(
        bucket_dir.join("V20260301000000__add_baz.sql"),
        "ALTER TABLE foo ADD COLUMN baz TEXT;",
    )
    .unwrap();

    let migrations_root = work.join("migrations");
    let bare_remote = std::env::temp_dir().join(format!(
        "djogi-t7-bare-{}.git",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&bare_remote);

    // Bare remote.
    let bare_init = std::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("-b")
        .arg("main")
        .arg(&bare_remote)
        .output()
        .expect("git init bare");
    assert!(
        bare_init.status.success(),
        "git init --bare failed: {}",
        String::from_utf8_lossy(&bare_init.stderr)
    );

    // Working repo at migrations/.
    let init = std::process::Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .arg(&migrations_root)
        .output()
        .expect("git init");
    assert!(init.status.success());

    for (k, v) in [("user.email", "t7@kindnudge.app"), ("user.name", "T7")] {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("config")
            .arg(k)
            .arg(v)
            .output()
            .expect("git config");
    }
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(&bare_remote)
        .output()
        .expect("git remote add");

    // Initial commit + push so origin has the branch.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("add")
        .arg(".")
        .output()
        .expect("git add");
    let initial_commit = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("commit")
        .arg("-m")
        .arg("initial")
        .output()
        .expect("git commit");
    assert!(
        initial_commit.status.success(),
        "initial git commit failed: {}",
        String::from_utf8_lossy(&initial_commit.stderr)
    );
    let push_initial = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("push")
        .arg("-u")
        .arg("origin")
        .arg("main")
        .output()
        .expect("git push initial");
    assert!(
        push_initial.status.success(),
        "initial push failed: {}",
        String::from_utf8_lossy(&push_initial.stderr)
    );

    // Seed ledger rows for ALL three versions (the retained `from`
    // row is needed for the B-4 checksum refresh; the two later rows
    // get DELETEd by squash).
    djogi::migrate::bootstrap_ledger(&mut ctx)
        .await
        .expect("bootstrap");
    for v in [
        "V20260101000000__init",
        "V20260201000000__add_bar",
        "V20260301000000__add_baz",
    ] {
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, checksum_down, execution_mode, status, \
              run_id, snapshot_version, app_label) \
             VALUES ($1, $2, $3, NULL, 'transactional', 'applied', 0, '1.0', '')",
            &[
                &v.to_string(),
                &"seed".to_string(),
                &"V1:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            ],
        )
        .await
        .expect("seed");
    }

    // Capture the pre-attune HEAD so we can prove `--publish` advanced
    // the local branch. The initial commit pushed above is the only
    // commit on `main` at this point.
    let pre_attune_head = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("rev-parse pre-attune")
            .stdout,
    )
    .trim()
    .to_string();

    // Round-2 A-1: a single `attune --squash --publish` invocation
    // performs the squash mutation, auto-commits the result, and pushes
    // to `origin`. The operator does NOT run `git add` / `git commit`
    // between the squash mutation and the push — the publisher owns
    // the commit-then-push contract end-to-end.
    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: true,
            app: None,
        },
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = match attune(&mut ctx, req).await {
        Ok(r) => r,
        Err(djogi::migrate::AttuneError::GitPublishFailed {
            stderr,
            status_code,
        }) => {
            // Surface captured stderr so a CI-side failure is
            // debuggable without re-running.
            panic!("attune --publish: GitPublishFailed (status={status_code:?}): {stderr}");
        }
        Err(other) => panic!("unexpected attune error: {other}"),
    };
    drop(guard);

    // (1) The report must mark the run as both mutated AND published —
    // those are the publisher's two contract bits.
    assert!(report.mutated, "squash must produce a rewrite");
    assert!(
        report.published,
        "report.published must be TRUE after a successful --publish run"
    );

    // (2) The local migrations submodule's HEAD must have advanced
    // past the pre-attune state — the publisher's auto-commit IS the
    // proof that the squash mutation was committed before pushing.
    let post_attune_local_head = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("rev-parse post-attune local")
            .stdout,
    )
    .trim()
    .to_string();
    assert_ne!(
        pre_attune_head, post_attune_local_head,
        "attune --publish must auto-commit the squash mutation; local HEAD \
         did not advance past the pre-attune commit ({pre_attune_head})"
    );

    // (3) The bare remote's HEAD must equal the post-attune local HEAD
    // — the publisher pushed the squash commit it just authored.
    let remote_head = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(&bare_remote)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("rev-parse bare")
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(
        remote_head, post_attune_local_head,
        "bare remote HEAD ({remote_head}) must equal the post-attune local HEAD \
         ({post_attune_local_head}); attune --publish is contracted to commit + push \
         the squash mutation atomically"
    );

    // (4) The auto-commit's message must be the canonical
    // `djogi attune --squash from <from>` shape so the audit trail in
    // the migrations submodule's history points back to the operator's
    // intent.
    let commit_subject = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("log")
            .arg("-1")
            .arg("--pretty=%s")
            .output()
            .expect("git log post-attune")
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(
        commit_subject, "djogi attune --squash from V20260101000000__init",
        "auto-commit subject must name the canonical `from` version"
    );

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&bare_remote);
}

// ── localhost predicate spot check at integration level ───────────────────

#[djogi::djogi_test]
async fn is_localhost_connection_pins_byte_grammar(_ctx: djogi::DjogiContext) {
    // Every URL form the test environment might produce must satisfy
    // the localhost predicate. The harness configures DATABASE_URL to
    // a localhost-pointing Postgres, so we explicitly pin both forms
    // here as integration-level proof that the policy wiring lines up
    // with the harness URLs.
    assert!(is_localhost_connection("postgres://localhost/test"));
    assert!(is_localhost_connection("postgres://127.0.0.1/test"));
    assert!(is_localhost_connection("host=localhost dbname=test"));
    assert!(!is_localhost_connection("postgres://prod.example.com/test"));
}

// ── Cross-DB FK rejection at projection time ──────────────────────────────

#[test]
fn cross_database_fk_rejected_at_projection_integration_smoke() {
    use djogi::migrate::ProjectionError;
    use djogi::migrate::projection::BucketKey;

    // The projection layer's own unit test in
    // `djogi/src/migrate/projection.rs` covers this contract with
    // synthetic descriptors. The integration-level smoke here pins
    // that the `CrossDatabaseForeignKey` error variant is actually
    // exposed in the public API so adopters can match on it.
    let _v: ProjectionError = ProjectionError::CrossDatabaseForeignKey {
        source_bucket: BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        },
        source_table: "invoices".to_string(),
        source_column: "audit_id".to_string(),
        target_bucket: BucketKey {
            database: "crud_log".to_string(),
            app: "audit".to_string(),
        },
        target_table: "audit_rows".to_string(),
    };
    let msg = format!("{_v}");
    assert!(msg.contains("cross-database"), "{msg}");
    assert!(msg.contains("invoices"), "{msg}");
    assert!(msg.contains("audit_rows"), "{msg}");
}

// ── Codex umbrella U-1: target / --apply / --record live coverage ────────

/// Helper — initialise a parent git repo with a `migrations/` submodule
/// (working repo) at the supplied workspace root. Returns the SHA of
/// the migrations submodule's initial commit so tests can resolve
/// targets against it.
fn init_parent_with_migrations_submodule(work: &std::path::Path, db: &str) -> String {
    use std::process::Command;
    // Parent repo init.
    let init_parent = Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .arg(work)
        .output()
        .expect("git init parent");
    assert!(init_parent.status.success());
    for (k, v) in [("user.email", "u1@kindnudge.app"), ("user.name", "U1")] {
        let _ = Command::new("git")
            .arg("-C")
            .arg(work)
            .arg("config")
            .arg(k)
            .arg(v)
            .output();
    }

    // Migrations working repo.
    let migrations_root = work.join("migrations");
    let init_migs = Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .arg(&migrations_root)
        .output()
        .expect("git init migrations");
    assert!(init_migs.status.success());
    for (k, v) in [("user.email", "u1@kindnudge.app"), ("user.name", "U1")] {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("config")
            .arg(k)
            .arg(v)
            .output();
    }

    // Lay one SQL file under `<active_db>/_global_/` so attune has
    // something to scan. Commit it so the migrations submodule has a
    // resolvable HEAD SHA.
    let bucket_dir = migrations_root.join(db).join("_global_");
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260101000000__init.sql"),
        "CREATE TABLE u1_target_widget (id BIGINT PRIMARY KEY);",
    )
    .unwrap();
    let _ = Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("add")
        .arg(".")
        .output()
        .expect("git add");
    let commit = Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("commit")
        .arg("-m")
        .arg("init migration history")
        .output()
        .expect("git commit");
    assert!(
        commit.status.success(),
        "migrations init commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("rev-parse HEAD");
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

    // Stage the submodule pointer in the parent so `git update-index
    // --cacheinfo` has something to overwrite. We use `git add` of a
    // gitlink to register the path.
    let _ = Command::new("git")
        .arg("-C")
        .arg(work)
        .arg("update-index")
        .arg("--add")
        .arg("--cacheinfo")
        .arg(format!("160000,{sha},migrations"))
        .output()
        .expect("seed parent submodule pointer");

    sha
}

/// `git_available()` — reuse the existing helper above. (No-op stub
/// here so the test reads naturally; the real impl is at line ~1340.)
fn u1_git_available() -> bool {
    git_available()
}

/// Codex umbrella U-1: when `target` is supplied, attune resolves it
/// against the local migrations submodule. The resolved SHA is
/// surfaced in `report.resolved_target`.
#[djogi::djogi_test]
async fn u1_attune_resolves_local_target(mut ctx: djogi::DjogiContext) {
    if !u1_git_available() {
        eprintln!("[u1] skipping u1_attune_resolves_local_target: git not on PATH");
        return;
    }
    let work = temp_workspace("u1_target_local");
    let db = current_database(&mut ctx).await;
    let initial_sha = init_parent_with_migrations_submodule(&work, &db);

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: Some(initial_sha.as_str()),
        apply: false, // dry-run
        record: false,
        dev_mode: true,
        mode: AttuneMode::DiffOnly,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert_eq!(
        report.resolved_target,
        Some(initial_sha.clone()),
        "resolved_target must echo the local SHA"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-1: a target that does not exist locally AND has
/// no remote configured surfaces `GitFetchFailed` after the local
/// rev-parse fails. (`git fetch --all` exits 0 with no remotes, so we
/// hit the retry path's `GitTargetResolveFailed` instead. We exercise
/// THAT path here.)
#[djogi::djogi_test]
async fn u1_attune_missing_target_surfaces_typed_error(mut ctx: djogi::DjogiContext) {
    if !u1_git_available() {
        eprintln!("[u1] skipping u1_attune_missing_target_surfaces_typed_error: git not on PATH");
        return;
    }
    let work = temp_workspace("u1_target_missing");
    let db = current_database(&mut ctx).await;
    let _initial = init_parent_with_migrations_submodule(&work, &db);

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: Some("nonexistent_branch_or_sha"),
        apply: false,
        record: false,
        dev_mode: true,
        mode: AttuneMode::DiffOnly,
        _guard: &guard,
    };
    let err = attune(&mut ctx, req).await.expect_err("must error");
    match err {
        AttuneError::GitTargetResolveFailed { target, .. } => {
            assert_eq!(target, "nonexistent_branch_or_sha");
        }
        // Acceptable alternative: if `git fetch --all` itself errors
        // (e.g. CI sandbox has constrained git binary), we surface
        // `GitFetchFailed`. Both flow to exit 1.
        AttuneError::GitFetchFailed { .. } => {}
        other => panic!("expected GitTargetResolveFailed or GitFetchFailed, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-1: `--apply false` must keep Record mode read-only.
/// The `Unrecorded` entry is reported but no ledger row is inserted.
#[djogi::djogi_test]
async fn u1_attune_record_without_apply_is_dry_run(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u1_record_dry_run");
    let db = current_database(&mut ctx).await;
    // Lay an unrecorded SQL file.
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260101000003__dry_run.sql"),
        "CREATE TABLE u1_dry_table(id INT);",
    )
    .unwrap();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: None,
        apply: false, // the gate under test
        record: false,
        dev_mode: true,
        mode: AttuneMode::Record {
            reason: "u1 dry-run test".to_string(),
        },
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(!report.mutated, "dry-run must not mutate");
    // The DryRunMutationsSkipped diagnostic must be present.
    assert!(
        report.diagnostics.iter().any(|d| matches!(
            d,
            djogi::migrate::AttuneDiagnostic::DryRunMutationsSkipped { mode } if *mode == "Record"
        )),
        "must surface DryRunMutationsSkipped: {:?}",
        report.diagnostics
    );
    // CRITICAL: ledger has NO row for the unrecorded version. Under
    // Codex umbrella U-5 the dry-run path also does not bootstrap the
    // ledger table on a fresh per-test DB — so the absence-of-row
    // assertion takes the form "either the table is missing entirely
    // (no bootstrap on dry-run, the U-5 invariant) or the table is
    // present but the version is absent". Both shapes prove no row
    // was inserted on this run; the U-5 dedicated tests pin the
    // table-existence side directly.
    let table_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("table-exists probe");
    if table_exists {
        let count: i64 = ctx
            .raw_scalar(
                "SELECT COUNT(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
                &[&"V20260101000003__dry_run".to_string()],
            )
            .await
            .expect("count");
        assert_eq!(count, 0, "dry-run must NOT insert a ledger row");
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-1: `--apply true` mutates as before. Same
/// fixture as the dry-run test; flipping the flag flips the outcome.
#[djogi::djogi_test]
async fn u1_attune_record_with_apply_mutates_ledger(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u1_record_apply");
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260101000004__apply.sql"),
        "CREATE TABLE u1_apply_table(id INT);",
    )
    .unwrap();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        mode: AttuneMode::Record {
            reason: "u1 apply test".to_string(),
        },
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(report.mutated, "--apply must mutate");
    let count: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
            &[&"V20260101000004__apply".to_string()],
        )
        .await
        .expect("count");
    assert_eq!(count, 1, "--apply must insert a ledger row");
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-1: `--record --apply` with a resolved target
/// updates the parent repo's recorded submodule pointer. This is the
/// load-bearing test for the umbrella verdict — pre-fix `--record`
/// only inserted ledger rows; the parent pointer was never written.
#[djogi::djogi_test]
async fn u1_attune_record_apply_updates_parent_submodule_pointer(mut ctx: djogi::DjogiContext) {
    if !u1_git_available() {
        eprintln!(
            "[u1] skipping u1_attune_record_apply_updates_parent_submodule_pointer: \
             git not on PATH"
        );
        return;
    }
    let work = temp_workspace("u1_record_pointer");
    let db = current_database(&mut ctx).await;
    let initial_sha = init_parent_with_migrations_submodule(&work, &db);

    // Make a SECOND commit on the migrations submodule so we can
    // attune the parent to the NEW SHA, distinct from the seeded
    // pointer. Append a second SQL file then commit.
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::write(
        bucket_dir.join("V20260201000000__second.sql"),
        "CREATE TABLE u1_second(id INT);",
    )
    .unwrap();
    let migrations_root = work.join("migrations");
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("add")
        .arg(".")
        .output()
        .expect("git add second");
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("commit")
        .arg("-m")
        .arg("second migration")
        .output()
        .expect("git commit second");
    let head_out = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("rev-parse HEAD second");
    let new_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    assert_ne!(initial_sha, new_sha, "second commit must produce a new SHA");

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: Some(new_sha.as_str()),
        apply: true,
        record: true,
        dev_mode: true,
        mode: AttuneMode::DiffOnly,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert_eq!(report.resolved_target, Some(new_sha.clone()));
    assert!(
        report.parent_pointer_updated,
        "parent submodule pointer must be updated"
    );

    // Verify the parent's index entry now records new_sha at the
    // `migrations` path (160000 mode). `git ls-files --stage` prints
    // `<mode> <sha> <stage>\t<path>`.
    let ls = std::process::Command::new("git")
        .arg("-C")
        .arg(&work)
        .arg("ls-files")
        .arg("--stage")
        .arg("migrations")
        .output()
        .expect("git ls-files");
    let stdout = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(
        stdout.contains(&new_sha),
        "parent index must record new_sha {new_sha} at migrations path; got: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-1: `--record` without `--apply` is a dry-run for
/// the parent pointer too. The DryRunRecordSkipped diagnostic
/// surfaces; the parent's recorded pointer stays unchanged.
#[djogi::djogi_test]
async fn u1_attune_record_without_apply_does_not_touch_parent(mut ctx: djogi::DjogiContext) {
    if !u1_git_available() {
        eprintln!(
            "[u1] skipping u1_attune_record_without_apply_does_not_touch_parent: \
             git not on PATH"
        );
        return;
    }
    let work = temp_workspace("u1_record_no_apply");
    let db = current_database(&mut ctx).await;
    let initial_sha = init_parent_with_migrations_submodule(&work, &db);

    // Capture the parent's recorded migrations pointer BEFORE the
    // attune call.
    let pre = std::process::Command::new("git")
        .arg("-C")
        .arg(&work)
        .arg("ls-files")
        .arg("--stage")
        .arg("migrations")
        .output()
        .expect("git ls-files pre");
    let pre_stdout = String::from_utf8_lossy(&pre.stdout).into_owned();

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: Some(initial_sha.as_str()),
        apply: false, // dry-run
        record: true,
        dev_mode: true,
        mode: AttuneMode::DiffOnly,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(
        !report.parent_pointer_updated,
        "dry-run must not write parent pointer"
    );
    assert!(
        report.diagnostics.iter().any(|d| matches!(
            d,
            djogi::migrate::AttuneDiagnostic::DryRunRecordSkipped { resolved_target, .. } if resolved_target.as_deref() == Some(initial_sha.as_str())
        )),
        "must surface DryRunRecordSkipped: {:?}",
        report.diagnostics
    );

    // Parent's recorded pointer is byte-identical to the pre-attune
    // state.
    let post = std::process::Command::new("git")
        .arg("-C")
        .arg(&work)
        .arg("ls-files")
        .arg("--stage")
        .arg("migrations")
        .output()
        .expect("git ls-files post");
    let post_stdout = String::from_utf8_lossy(&post.stdout).into_owned();
    assert_eq!(
        pre_stdout, post_stdout,
        "parent index entry for `migrations` must be unchanged after a dry-run"
    );
    let _ = std::fs::remove_dir_all(&work);
}

// ── Codex umbrella U-5: dry-run must NOT bootstrap ledger table ───────────

/// Codex umbrella U-5 BLOCK: `attune --record-ledger` (Record mode)
/// without `--apply` MUST NOT create `djogi_schema_migrations` on a
/// fresh database. Pre-fix the bootstrap call sat OUTSIDE the
/// `--apply` gate, so the dry-run silently created the ledger table —
/// an out-of-contract mutation that the umbrella round-2 review
/// caught.
///
/// We exercise the load-bearing invariant by:
///
/// 1. Asserting the ledger table is absent on the fresh per-test DB
///    (every `#[djogi_test]` gets a clean database).
/// 2. Running attune in Record mode with `apply: false`.
/// 3. Asserting `pg_class` STILL shows no `djogi_schema_migrations`.
#[djogi::djogi_test]
async fn u5_attune_record_dry_run_does_not_bootstrap_ledger(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u5_record_no_bootstrap");
    let db = current_database(&mut ctx).await;
    // Lay one unrecorded SQL file so the diff has something to walk.
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260501000000__u5_dry.sql"),
        "CREATE TABLE u5_dry_table(id INT);",
    )
    .unwrap();

    // Pre-condition: ledger does NOT exist yet on the fresh per-test DB.
    let pre_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("pre-probe");
    assert!(
        !pre_exists,
        "fresh per-test DB must not carry djogi_schema_migrations"
    );

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: None,
        apply: false, // the load-bearing flag
        record: false,
        dev_mode: true,
        mode: AttuneMode::Record {
            reason: "u5 dry-run".to_string(),
        },
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(!report.mutated, "dry-run must report mutated=false");

    // CRITICAL: ledger table must STILL be absent. Pre-U-5 the
    // bootstrap ran during this attune call and the table existed by
    // now — violating the read-only contract.
    let post_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("post-probe");
    assert!(
        !post_exists,
        "U-5: Record dry-run must NOT bootstrap djogi_schema_migrations"
    );

    // The LedgerTableMissing diagnostic must be present so the
    // operator sees why the diff is vacuous.
    assert!(
        report.diagnostics.iter().any(|d| matches!(
            d,
            djogi::migrate::AttuneDiagnostic::LedgerTableMissing { database } if database == &db
        )),
        "must surface LedgerTableMissing on dry-run with no ledger: {:?}",
        report.diagnostics
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-5: `attune --record-ledger --apply` on a fresh DB
/// MUST bootstrap the ledger before inserting the recorded row. The
/// flag flip recovers the pre-fix behavior on the apply path so we
/// don't regress the legitimate side of the gate.
#[djogi::djogi_test]
async fn u5_attune_record_apply_does_bootstrap_ledger(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u5_record_with_apply");
    let db = current_database(&mut ctx).await;
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260501000001__u5_apply.sql"),
        "CREATE TABLE u5_apply_table(id INT);",
    )
    .unwrap();

    let pre_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("pre-probe");
    assert!(!pre_exists, "fresh per-test DB must start empty");

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: None,
        apply: true,
        record: false,
        dev_mode: true,
        mode: AttuneMode::Record {
            reason: "u5 apply".to_string(),
        },
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(report.mutated, "--apply must mutate");

    let post_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("post-probe");
    assert!(
        post_exists,
        "U-5: Record --apply must bootstrap djogi_schema_migrations"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella U-5: `attune --squash` without `--apply` MUST NOT
/// create the ledger table. The squash dry-run path early-returns
/// before any disk mutation, but the bootstrap call sat upstream of
/// the dry-run gate — pre-fix it would still execute. We pin the
/// post-fix invariant here.
#[djogi::djogi_test]
async fn u5_attune_squash_dry_run_does_not_bootstrap_ledger(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("u5_squash_no_bootstrap");
    let db = current_database(&mut ctx).await;
    // A SQL file so the squash candidate-bucket scan has something
    // to find. We pass the version as `from`; without a matching
    // disk entry the squash would refuse with `SquashFromVersionNotFound`,
    // but we want to exercise the bootstrap-gate path independently
    // from the from-version gate.
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::create_dir_all(&bucket_dir).unwrap();
    std::fs::write(
        bucket_dir.join("V20260501000002__u5_squash_dry.sql"),
        "CREATE TABLE u5_squash_dry(id INT);",
    )
    .unwrap();

    let pre_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("pre-probe");
    assert!(!pre_exists, "fresh per-test DB must start empty");

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        target: None,
        apply: false, // dry-run
        record: false,
        dev_mode: true,
        mode: AttuneMode::Squash {
            from: "V20260501000002__u5_squash_dry".to_string(),
            publish: false,
            app: None,
        },
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(!report.mutated, "Squash dry-run must report mutated=false");

    let post_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("post-probe");
    assert!(
        !post_exists,
        "U-5: Squash dry-run must NOT bootstrap djogi_schema_migrations"
    );

    // DryRunMutationsSkipped surfaces with mode = "Squash" so the
    // operator's CLI output names the requested mode.
    assert!(
        report.diagnostics.iter().any(|d| matches!(
            d,
            djogi::migrate::AttuneDiagnostic::DryRunMutationsSkipped { mode } if *mode == "Squash"
        )),
        "must surface DryRunMutationsSkipped(mode=\"Squash\"): {:?}",
        report.diagnostics
    );
    let _ = std::fs::remove_dir_all(&work);
}

// ── Codex umbrella U-7: --squash implies recording ───────────────────────

/// Codex umbrella round-2 U-7: `--squash` clearly implies
/// `--record` per `docs/spec/configuration.md` §15 ("parent-repo
/// submodule-pointer changes are explicit via `--record` or options
/// that clearly imply recording, such as `--squash`") and
/// `docs/spec/migrations.md` §"migrations attune" ("a command mode
/// clearly implies recording, such as `--squash`").
///
/// Pre-U-7 the implementation only honoured the explicit `req.record`
/// flag — an operator running `attune --squash --apply --target <ref>`
/// saw the squash succeed but the parent's recorded submodule
/// pointer stayed put. This test pins the post-fix invariant: a
/// Squash-mode invocation with a resolved target writes the parent
/// pointer WITHOUT requiring the operator to also pass `--record`.
///
/// The squash itself can be a no-op (no later versions to subsume) —
/// we don't care about the squash mutation here. We care that the
/// recording side-effect fires on Squash mode regardless of whether
/// `req.record` was explicitly set.
#[djogi::djogi_test]
async fn u7_attune_squash_implies_recording_without_explicit_flag(mut ctx: djogi::DjogiContext) {
    if !u1_git_available() {
        eprintln!(
            "[u7] skipping u7_attune_squash_implies_recording_without_explicit_flag: \
             git not on PATH"
        );
        return;
    }
    let work = temp_workspace("u7_squash_implies_record");
    let db = current_database(&mut ctx).await;
    let initial_sha = init_parent_with_migrations_submodule(&work, &db);

    // Make a SECOND commit on the migrations submodule so we can
    // attune the parent to the NEW SHA distinct from the seeded
    // pointer (otherwise the assertion can't tell "no write" from
    // "wrote the same SHA back").
    let bucket_dir = work.join(format!("migrations/{db}/_global_"));
    std::fs::write(
        bucket_dir.join("V20260201000000__u7_second.sql"),
        "CREATE TABLE u7_second(id INT);",
    )
    .unwrap();
    let migrations_root = work.join("migrations");
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("add")
        .arg(".")
        .output()
        .expect("git add second");
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("commit")
        .arg("-m")
        .arg("u7 second migration")
        .output()
        .expect("git commit second");
    let head_out = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("rev-parse HEAD second");
    let new_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    assert_ne!(initial_sha, new_sha, "second commit must produce a new SHA");

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        // Squash from the FIRST seeded migration. With only that one
        // up file on disk in the seeded bucket, the squash is a
        // no-op (to_squash.len() <= 1 path) — but the parent-pointer
        // recording side-effect must still fire because Squash
        // implies --record.
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: Some(new_sha.as_str()),
        apply: true,
        // CRITICAL: --record is FALSE. Pre-U-7 this meant the parent
        // pointer would NOT be updated; post-U-7 Squash mode auto-
        // implies recording so the pointer DOES get written.
        record: false,
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert_eq!(report.resolved_target, Some(new_sha.clone()));
    assert!(
        report.parent_pointer_updated,
        "U-7: --squash with a resolved target must auto-write the parent \
         pointer even without explicit --record"
    );

    // Verify the parent's index entry now records new_sha at the
    // `migrations` path. Pre-U-7 this would still be initial_sha
    // because the recording side-effect was gated on req.record.
    let ls = std::process::Command::new("git")
        .arg("-C")
        .arg(&work)
        .arg("ls-files")
        .arg("--stage")
        .arg("migrations")
        .output()
        .expect("git ls-files");
    let stdout = String::from_utf8_lossy(&ls.stdout).into_owned();
    assert!(
        stdout.contains(&new_sha),
        "U-7: parent index must record new_sha {new_sha} at migrations path; got: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// Codex umbrella round-2 U-7: the dry-run side of the auto-imply
/// fires too — `attune --squash --target <ref>` (NO --apply, NO
/// --record) surfaces the `DryRunRecordSkipped` diagnostic. Pre-U-7
/// the diagnostic only surfaced when the operator typed `--record`
/// explicitly; the new effective-record computation makes Squash
/// mode behave as if `--record` were always present, both for the
/// apply-side mutation AND for the dry-run diagnostic surface.
#[djogi::djogi_test]
async fn u7_attune_squash_dry_run_surfaces_record_skipped_without_explicit_flag(
    mut ctx: djogi::DjogiContext,
) {
    if !u1_git_available() {
        eprintln!(
            "[u7] skipping u7_attune_squash_dry_run_surfaces_record_skipped: \
             git not on PATH"
        );
        return;
    }
    let work = temp_workspace("u7_squash_dry_record_skipped");
    let db = current_database(&mut ctx).await;
    let initial_sha = init_parent_with_migrations_submodule(&work, &db);

    let lock_path = work.join(".djogi-migrate.lock");
    let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(2)).expect("lock");
    let req = AttuneRequest {
        workspace_root: &work,
        database_url: "postgres://localhost/main",
        profile: "development",
        mode: AttuneMode::Squash {
            from: "V20260101000000__init".to_string(),
            publish: false,
            app: None,
        },
        target: Some(initial_sha.as_str()),
        apply: false,  // dry-run
        record: false, // and explicit record is OFF — Squash auto-implies
        dev_mode: true,
        _guard: &guard,
    };
    let report = attune(&mut ctx, req).await.expect("attune ok");
    assert!(!report.mutated, "dry-run must not mutate");
    assert!(
        !report.parent_pointer_updated,
        "dry-run must not write the parent pointer regardless of effective_record"
    );
    // The DryRunRecordSkipped diagnostic must surface even though
    // req.record was false — because Squash auto-implies recording.
    let squash_implied_diagnostic = report.diagnostics.iter().find(|d| {
        matches!(
            d,
            djogi::migrate::AttuneDiagnostic::DryRunSquashRecordSkipped { resolved_target }
                if resolved_target.as_deref() == Some(initial_sha.as_str())
        )
    });
    assert!(
        squash_implied_diagnostic.is_some(),
        "U-7 + U-9: Squash dry-run must surface DryRunRecordSkipped with the \
         resolved target SHA: {:?}",
        report.diagnostics
    );
    // Verify the rendered prose stays neutral across both the direct
    // `--record` and squash-implied code paths.
    let rendered = squash_implied_diagnostic.unwrap().to_string();
    assert!(
        rendered.contains(&format!(
            "would update parent submodule pointer to `{}` but `--apply` was not \
                 provided; no parent index mutation happened",
            initial_sha
        )),
        "U-9 prose must use the neutral wording: {rendered}"
    );
    assert!(
        !rendered.contains("--record requested"),
        "U-9 prose must NOT say `--record` requested: {rendered}"
    );
    let _ = std::fs::remove_dir_all(&work);
}
