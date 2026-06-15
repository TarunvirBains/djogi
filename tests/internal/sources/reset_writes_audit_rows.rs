// Issue #118 — `db reset` replay writes
// `djogi_ddl_audit` rows when `ResetRequest::audit_pool` is `Some(..)`.
//
// # What this test proves
//
// Pre-fix:
// - `RunnerCtx::audit_pool` plumbing already existed
//  so `apply_plan` could write per-segment audit rows when given a
//  `Some(pool)`.
// - The only production constructor of `RunnerCtx`
//  (`reset::replay_one_migration`) hard-coded `audit_pool: None`, so
//  no production `db reset` invocation ever wrote a row to
//  `djogi_ddl_audit`.
// - The CLI's `db reset` glue did not resolve a `crud_log_url` pool
//  either; the field on `RunnerCtx` was unreachable from production
//  dispatch.
//
// Post-fix:
// - `ResetRequest` carries an `audit_pool: Option<deadpool_postgres::Pool>`.
// - `replay_one_migration` threads it through to `RunnerCtx::audit_pool`.
// - The CLI's `run_reset` builds the pool best-effort via the new
//  shared `djogi::migrate::resolve_audit_url` + `build_audit_pool`
//  helpers.
//
// This test exercises the library wiring end-to-end: it provisions a
// virgin DB, composes a bootstrap migration, calls `reset_app_database`
// with `audit_pool: Some(pool_to_same_db)` (single-DB simplification
// mirroring the .7 verify CLI integration test), and then asserts
// that `djogi_ddl_audit` exists post-reset and carries at least one
// row pointing at the replayed migration. It also verifies the
// negative case: passing `audit_pool: None` (the pre-fix shape) leaves
// `djogi_ddl_audit` absent, matching the runner's silent-skip
// contract.
//
// # Why a virgin per-test DB
//
// `reset_app_database` drops + recreates the application database via
// the maintenance connection (`DROP DATABASE WITH (FORCE)`). The
// harness's `#[djogi_test]` per-test DB lives at `djogi_test_<uuid>`
// — dropping it from inside the test would yank the connection the
// test is using. We follow the established
// `zero_db_reset_replays_phase_zero` pattern: open an
// independent admin connection, `CREATE DATABASE` a sibling
// `replay_<stamp>`, run reset against it, then
// drop the virgin DB in cleanup.
//
// # Single-DB simplification (vs. the spec's two-DB model)
//
// Production splits `crud_log_url` out from `database.url` so
// `db reset` (which targets the app DB) cannot erase the audit trail.
// Provisioning a sibling audit DB inside this test would require a
// second admin URL OR a harness extension; the verify CLI integration
// test (`djogi_verify_cli`) made the same call and pointed both
// URLs at the same per-test database. We follow that precedent: the
// audit pool here points at the SAME virgin DB the reset replays
// against. `djogi_ddl_audit` is a namespaced table inside that DB so
// the wire-up still demonstrates that the audit row is written by the
// production replay path. Production `crud_log_url` separation
// remains a topology concern outside this library-level test.
//
// # Spec / memory anchors
//
// - GH issue #118.
// - CLAUDE.md "Three-Database Architecture".
// - v3 plan §453 (audit table schema), §469.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use djogi::config::MigrateConfig;
use djogi::migrate::{
  AppLifecycle, AppliedSchema, BucketKey, ComposeRequest, GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME,
  PHASE_ZERO_VERSION, ResetRequest, RunnerIdentity, SNAPSHOT_FORMAT_VERSION, WorkspaceGuard,
  acquire_workspace_lock, build_audit_pool, compose, reset_app_database,
};
use tokio_postgres::NoTls;

// ── Helpers ───────────────────────────────────────────────────────────────

fn temp_workspace(label: &str) -> PathBuf {
  let stamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos();
  let path = std::env::temp_dir().join(format!("djogi-audit-replay-{label}-{stamp}"));
  fs::create_dir_all(&path).expect("create workspace root");
  path
}

fn lock_for(workspace: &Path) -> WorkspaceGuard {
  let lock_path = workspace.join(LOCK_FILE_NAME);
  acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT).expect("acquire workspace lock")
}

fn empty_schema_for(bucket: &BucketKey) -> AppliedSchema {
  AppliedSchema {
    djogi_version: "0.1.0".to_string(),
    enums: BTreeMap::new(),
    format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
    generated_at: "2026-05-09T00:00:00Z".to_string(),
    indexes: Vec::new(),
    models: BTreeMap::new(),
    registered_apps: vec![bucket.app.clone()],
  }
}

fn at(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> time::OffsetDateTime {
  let date =
    time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day).unwrap();
  let t = time::Time::from_hms(hour, minute, second).unwrap();
  date.with_time(t).assume_utc()
}

/// Replace the database name in a Postgres URL — duplicated from
/// the sibling reset replay test so this
/// fixture does not reach into pub(crate) helpers.
fn replace_db_in_url(url: &str, new_db: &str) -> String {
  let body = url
    .strip_prefix("postgres://")
    .or_else(|| url.strip_prefix("postgresql://"))
    .expect("postgres URL");
  let mut idx = 0usize;
  let body_bytes = body.as_bytes();
  while idx < body_bytes.len() && body_bytes[idx] != b'/' {
    idx += 1;
  }
  let scheme_end = url.len() - body.len();
  let prefix = &url[..scheme_end + idx + 1];
  let rest = &body[idx + 1..];
  let mut path_end = 0usize;
  let rest_bytes = rest.as_bytes();
  while path_end < rest_bytes.len() && rest_bytes[path_end] != b'?' {
    path_end += 1;
  }
  let suffix = &rest[path_end..];
  format!("{prefix}{new_db}{suffix}")
}

/// Provision a virgin Postgres database and return `(virgin_db_name,
/// virgin_db_url)`. Uses the harness's `DATABASE_URL` admin
/// credentials.
async fn provision_virgin_db(label: &str) -> (String, String) {
  let admin_url = std::env::var("DATABASE_URL")
    .expect("DATABASE_URL env var must be set for live integration tests");
  let stamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos();
  let virgin_db = format!("djogi_audit_replay_{label}_{stamp}");

  let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
    .await
    .expect("admin connect");
  let admin_driver = tokio::spawn(async move {
    if let Err(e) = admin_conn.await {
      eprintln!("[issue #118] admin driver: {e}");
    }
  });
  admin_client
    .batch_execute(&format!("CREATE DATABASE \"{virgin_db}\""))
    .await
    .expect("CREATE virgin DATABASE");
  drop(admin_client);
  let _ = admin_driver.await;

  let virgin_url = replace_db_in_url(&admin_url, &virgin_db);
  (virgin_db, virgin_url)
}

/// Drop a virgin DB created via [`provision_virgin_db`]. Best-effort —
/// teardown failures are logged but do not panic, so a test failure
/// surfaces the real assertion.
async fn drop_virgin_db(virgin_db: &str) {
  let admin_url = match std::env::var("DATABASE_URL") {
    Ok(u) => u,
    Err(_) => return,
  };
  let (admin_client, admin_conn) = match tokio_postgres::connect(&admin_url, NoTls).await {
    Ok(c) => c,
    Err(e) => {
      eprintln!("[issue #118] teardown connect failed: {e}");
      return;
    }
  };
  let teardown_driver = tokio::spawn(async move {
    if let Err(e) = admin_conn.await {
      eprintln!("[issue #118] teardown driver: {e}");
    }
  });
  let _ = admin_client
    .batch_execute(&format!(
      "DROP DATABASE IF EXISTS \"{virgin_db}\" WITH (FORCE)"
    ))
    .await;
  drop(admin_client);
  let _ = teardown_driver.await;
}

/// Compose against `virgin_db` so the workspace carries one
/// committed migration the reset path can replay.
fn compose_test_schema(work: &Path, virgin_db: &str) {
  let guard = lock_for(work);
  let bucket = BucketKey {
    database: virgin_db.to_string(),
    app: String::new(),
  };
  let mut models = BTreeMap::new();
  models.insert(bucket.clone(), empty_schema_for(&bucket));
  let mut snapshots = BTreeMap::new();
  snapshots.insert(bucket.clone(), empty_schema_for(&bucket));
  let apps = vec![AppLifecycle {
    label: String::new(),
    database: virgin_db.to_string(),
    renamed_from: None,
    tombstone: false,
  }];

  let req = ComposeRequest {
    workspace_root: work,
    models: &models,
    snapshots: &snapshots,
    apps: &apps,
    name: "test_schema_for_audit_wire_up",
    allow_destructive: false,
    force_overwrite: false,
    now: at(2026, 5, 9, 12, 0, 0),
    _guard: &guard,
    pk_flip_join_table_option: None,
    skip_phase_zero_auto_emit: false,
  };
  let report = compose(req).expect("compose with auto-emit");
  assert_eq!(
    report.emitted_phase_zero.len(),
    1,
    "bootstrap migration must be auto-emitted for the virgin DB"
  );
  drop(guard);
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Positive — `audit_pool: Some(pool)` causes the replay path to
/// write `djogi_ddl_audit` rows. THIS is the regression test for
/// issue #118: pre-fix the only production constructor of `RunnerCtx`
/// passed `audit_pool: None`, so no real `db reset` ever wrote an
/// audit row.
#[tokio::test]
async fn db_reset_with_audit_pool_writes_djogi_ddl_audit_rows() {
  // 1. Provision the virgin DB the reset will drop + recreate.
  let (virgin_db, virgin_url) = provision_virgin_db("with_pool").await;

  // 2. Build a workspace and compose for that virgin DB.
  let work = temp_workspace("with_pool");
  compose_test_schema(&work, &virgin_db);

  // 3. Build the audit pool against the SAME per-test DB
  //  (single-DB simplification — see the module-level comment).
  //  Pre-fix this pool would have been ignored even if the caller
  //  constructed it, because `replay_one_migration` hard-coded
  //  `audit_pool: None`.
  let audit_pool = build_audit_pool(&virgin_url)
    .await
    .expect("build audit pool against the virgin DB");

  // 4. Call `reset_app_database` with `audit_pool: Some(pool)`.
  //  Replay must now write one row per executed segment per
  //  replayed migration; `djogi_ddl_audit` must exist post-reset.
  let req = ResetRequest {
    database_url: &virgin_url,
    maintenance_database: "postgres",
    workspace_root: &work,
    profile: "test",
    confirmed: true,
    allow_checksum_drift_reset: false,
    migrate_config: MigrateConfig::default(),
    audit_pool: Some(audit_pool),
    runner_identity: Some(RunnerIdentity::SingleNodeDev),
  };
  let reset_report = reset_app_database(req)
    .await
    .expect("reset must succeed against virgin DB");
  assert!(
    reset_report
      .replayed_versions
      .iter()
      .any(|v| v.version == PHASE_ZERO_VERSION),
    "bootstrap migration must be replayed"
  );

  // 5. Assert the audit table was bootstrapped + populated.
  let (client, conn) = tokio_postgres::connect(&virgin_url, NoTls)
    .await
    .expect("connect to bootstrapped DB");
  let driver = tokio::spawn(async move {
    if let Err(e) = conn.await {
      eprintln!("[issue #118] post-reset driver: {e}");
    }
  });

  // 5a. The audit table exists. Pre-fix this would be `None` because
  //   the runner's `record_ddl_audit_for_plan` short-circuits on
  //   `audit_pool.is_none()` before calling `bootstrap_ddl_audit`.
  let audit_relname: Option<String> = client
    .query_one("SELECT to_regclass('public.djogi_ddl_audit')::text", &[])
    .await
    .expect("query audit table existence")
    .get(0);
  assert_eq!(
    audit_relname.as_deref(),
    Some("djogi_ddl_audit"),
    "audit table MUST be bootstrapped when ResetRequest::audit_pool is Some(..)"
  );

  // 5b. At least one row was written, and it points at the virgin
  //   DB with the empty (global) app label. The migration
  //   ships at least one transactional segment so we expect >= 1
  //   row, not exactly 1.
  let row_count: i64 = client
    .query_one(
      "SELECT COUNT(*)::bigint FROM djogi_ddl_audit \
       WHERE target_database = $1 AND app_label = $2",
      &[&virgin_db, &""],
    )
    .await
    .expect("count audit rows for virgin DB")
    .get(0);
  assert!(
    row_count >= 1,
    "expected at least one djogi_ddl_audit row after bootstrap migration replay; got {row_count}"
  );

  // 5c. The audit row carries the actual replayed DDL (not a
  //   placeholder). This catches a wiring bug where the runner
  //   would write rows but with empty / wrong SQL — proving the
  //   plan's segment SQL flows through `record_ddl_audit_for_plan`
  //   to the audit row's `ddl_sql` column.
  let ddl_sql: String = client
    .query_one(
      "SELECT ddl_sql FROM djogi_ddl_audit \
       WHERE target_database = $1 AND app_label = $2 \
       ORDER BY id ASC LIMIT 1",
      &[&virgin_db, &""],
    )
    .await
    .expect("read first audit row's ddl_sql")
    .get(0);
  assert!(
    !ddl_sql.is_empty(),
    "audit row must carry the replayed SQL, not an empty placeholder"
  );

  drop(client);
  let _ = driver.await;

  // 6. Cleanup.
  drop_virgin_db(&virgin_db).await;
  let _ = fs::remove_dir_all(&work);
}

/// Negative — `audit_pool: None` (the pre-fix shape, also the supported
/// "no audit DB provisioned" deployment) leaves `djogi_ddl_audit`
/// absent post-reset. Pins the runner's silent-skip contract so a
/// future refactor that always-bootstraps the audit table trips this
/// test.
///
/// This is the load-bearing companion to the positive case: without
/// it, a regression that flips the runner to "always bootstrap" would
/// pass the positive test but silently break the deployment shape
/// where the operator has not provisioned the audit DB at all.
#[tokio::test]
async fn db_reset_without_audit_pool_leaves_audit_table_absent() {
  let (virgin_db, virgin_url) = provision_virgin_db("no_pool").await;
  let work = temp_workspace("no_pool");
  compose_test_schema(&work, &virgin_db);

  let req = ResetRequest {
    database_url: &virgin_url,
    maintenance_database: "postgres",
    workspace_root: &work,
    profile: "test",
    confirmed: true,
    allow_checksum_drift_reset: false,
    migrate_config: MigrateConfig::default(),
    audit_pool: None,
    runner_identity: Some(RunnerIdentity::SingleNodeDev),
  };
  let _reset_report = reset_app_database(req)
    .await
    .expect("reset must succeed even without audit pool");

  let (client, conn) = tokio_postgres::connect(&virgin_url, NoTls)
    .await
    .expect("connect to bootstrapped DB");
  let driver = tokio::spawn(async move {
    if let Err(e) = conn.await {
      eprintln!("[issue #118] post-reset driver: {e}");
    }
  });

  let audit_relname: Option<String> = client
    .query_one("SELECT to_regclass('public.djogi_ddl_audit')::text", &[])
    .await
    .expect("query audit table existence")
    .get(0);
  assert_eq!(
    audit_relname, None,
    "audit table MUST stay absent when ResetRequest::audit_pool is None — \
     the runner's silent-skip contract is load-bearing for adopters who \
     have not provisioned the second DB"
  );

  drop(client);
  let _ = driver.await;
  drop_virgin_db(&virgin_db).await;
  let _ = fs::remove_dir_all(&work);
}

