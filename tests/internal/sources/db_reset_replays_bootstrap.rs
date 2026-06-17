// `db reset` replays the
// auto-emitted bootstrap migration end-to-end against a
// virgin Postgres database.
//
// # What this proves
//
// - A workspace whose `migrations/` tree was just produced by
//   `compose` (auto-emitting bootstrap migration) can be replayed against a
//   virgin database via `reset_app_database` — without manual
//   HeeRanjID schema install, without manual `CREATE EXTENSION`,
//   without any other side-channel install path.
// - Post-replay, explicit `SingleNodeDev` reset replay provisions
//   node 1 and `generate_id()` is callable on a new connection.
// - The `djogi_schema_migrations` ledger carries the //   version row marked `applied`.
//
// Together these prove the lockdown contract: production `db reset`
// replays the identity-free auto-emitted migration on a virgin DB
// with no parallel install path needed, while the explicit
// `SingleNodeDev` reset identity provisions node 1 after Phase 0 SQL
// succeeds.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use djogi::config::MigrateConfig;
use djogi::migrate::{
    AppLifecycle, AppliedSchema, BucketKey, ComposeRequest, GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME,
    PHASE_ZERO_VERSION, ResetError, ResetRefusal, ResetRequest, ResetSqlSide, RunnerIdentity,
    SNAPSHOT_FORMAT_VERSION, WorkspaceGuard, acquire_workspace_lock, compose, reset_app_database,
};
use tokio_postgres::NoTls;

// ── Helpers ───────────────────────────────────────────────────────────────

fn temp_workspace(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_dir_canon = std::env::temp_dir().canonicalize().expect("canonicalize temp dir");
    let path = temp_dir_canon.join(format!("djogi-bootstrap-replay-{label}-{stamp}"));
    let path = djogi::migrate::resolve_write_workspace_path(&temp_dir_canon, &path)
        .expect("resolve workspace root");
    djogi::migrate::create_workspace_parent_dirs(&temp_dir_canon, path.join(".keep"))
        .expect("create workspace root");
    if let Ok(path_canon) = std::fs::canonicalize(&path) {
        assert!(
            path_canon.starts_with(&temp_dir_canon),
            "workspace path escapes temp directory"
        );
    }
    path
}

fn safe_remove_workspace(path: &Path) {
    if let Ok(temp_canon) = std::env::temp_dir().canonicalize()
        && let Ok(path_canon) = djogi::migrate::resolve_existing_workspace_path(&temp_canon, path)
    {
        let _ = djogi::migrate::remove_workspace_dir_all(&temp_canon, &path_canon);
    }
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
        generated_at: "2026-05-04T00:00:00Z".to_string(),
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

/// Replace the database name in a Postgres URL.
///
/// Mirrors `djogi::migrate::reset::replace_db_in_url` — duplicated
/// here so the test doesn't reach into pub(crate) helpers.
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
    // Skip past the path component to preserve any query string.
    let mut path_end = 0usize;
    let rest_bytes = rest.as_bytes();
    while path_end < rest_bytes.len() && rest_bytes[path_end] != b'?' {
        path_end += 1;
    }
    let suffix = &rest[path_end..];
    format!("{prefix}{new_db}{suffix}")
}

// ── Test ──────────────────────────────────────────────────────────────────

/// Exercise the full bootstrap-migration contract end-to-end:
///
/// 1. Provision a virgin Postgres database (skipping the harness's
///    `setup_test_db_with_extensions` — that path will be re-routed
///    in sub-step 0.4; here we want to prove the bare-virgin-DB case).
/// 2. Build a workspace with `compose` → auto-emits bootstrap migration.
/// 3. Call `reset_app_database` against the virgin DB → replays
///    Bootstrap migration.
/// 4. Connect to the freshly-bootstrapped DB and assert the
///    HeeRanjID schema is present and functional.
#[tokio::test]
async fn db_reset_replays_phase_zero_against_virgin_database() {
    // 1. Provision a virgin DB alongside the harness's per-test DB.
    //    The "admin URL" is the env var DATABASE_URL — same as the
    //    rest of the live test suite.
    let admin_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL env var must be set for live integration tests");

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let virgin_db = format!("djogi_replay_{stamp}");

    {
        let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
            .await
            .expect("admin connect");
        let admin_driver = tokio::spawn(async move {
            if let Err(e) = admin_conn.await {
                eprintln!("[bootstrap replay] admin driver: {e}");
            }
        });
        admin_client
            .batch_execute(&format!("CREATE DATABASE \"{virgin_db}\""))
            .await
            .expect("CREATE virgin DATABASE");
        drop(admin_client);
        let _ = admin_driver.await;
    }

    // 2. Build a workspace and compose for the virgin DB.
    //    The bucket targets database == virgin_db so the auto-emit
    //    fires for the right database name (matches what the runner
    //    will use when issuing ALTER DATABASE).
    let work = temp_workspace("db_reset_replay");
    let guard = lock_for(&work);
    let bucket = BucketKey {
        database: virgin_db.clone(),
        app: String::new(),
    };
    let mut models = BTreeMap::new();
    models.insert(bucket.clone(), empty_schema_for(&bucket));
    let mut snapshots = BTreeMap::new();
    snapshots.insert(bucket.clone(), empty_schema_for(&bucket));
    let apps = vec![AppLifecycle {
        label: String::new(),
        database: virgin_db.clone(),
        renamed_from: None,
        tombstone: false,
    }];

    let req = ComposeRequest {
        workspace_root: &work,
        models: &models,
        snapshots: &snapshots,
        apps: &apps,
        name: "phase_zero_replay_test",
        allow_destructive: false,
        force_overwrite: false,
        now: at(2026, 5, 4, 12, 0, 0),
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

    // Drop the workspace lock guard so `reset_app_database` can
    // re-acquire it.
    drop(guard);

    // 3. Replay via `reset_app_database`. This drops +
    //    recreates the DB (via the maintenance URL, default
    //    "postgres") then replays every committed migration in
    //    lexical order — the bootstrap migration's all-zero timestamp guarantees it
    //    runs first.
    let virgin_url = replace_db_in_url(&admin_url, &virgin_db);
    let req = ResetRequest {
        database_url: &virgin_url,
        maintenance_database: "postgres",
        workspace_root: &work,
        profile: "test",
        confirmed: true,
        allow_checksum_drift_reset: false,
        migrate_config: MigrateConfig::default(),
        // Replay coverage does not assert audit-row behaviour;
        // dedicated coverage lives in
        // `tests/internal/sources/c2_118_*` per issue #118.
        audit_pool: None,
        runner_identity: Some(RunnerIdentity::SingleNodeDev),
    };
    let reset_report = reset_app_database(req)
        .await
        .expect("reset must succeed against virgin DB");
    assert_eq!(reset_report.database, virgin_db);
    assert!(
        reset_report
            .replayed_versions
            .iter()
            .any(|v| v.version == PHASE_ZERO_VERSION),
        "ledger must record bootstrap migration as replayed"
    );

    // 4. Connect to the freshly-bootstrapped DB and assert the
    //    HeeRanjID schema is present, node 1 is provisioned, and
    //    generated IDs work on a new connection.
    let (client, conn) = tokio_postgres::connect(&virgin_url, NoTls)
        .await
        .expect("connect to bootstrapped DB");
    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("[bootstrap replay] post-reset driver: {e}");
        }
    });

    // 4a. `heer_nodes` table exists.
    let heer_nodes_exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = 'heer_nodes' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("heer_nodes existence query")
        .get(0);
    assert!(
        heer_nodes_exists,
        "bootstrap migration must have created `heer_nodes` table"
    );

    // 4b. The explicit SingleNodeDev replay provisions node 1.
    let seeded_node: i32 = client
        .query_one("SELECT node_id FROM heer_nodes WHERE node_id = 1", &[])
        .await
        .expect("seeded node query")
        .get(0);
    assert_eq!(seeded_node, 1, "SingleNodeDev reset must provision node 1");

    // 4c. `generate_id()` is callable because reset replay persisted
    //     the SingleNodeDev database defaults.
    let row = client
        .query_one("SELECT generate_id() AS id", &[])
        .await
        .expect("generate_id() must be callable after SingleNodeDev reset replay");
    let id: i64 = row.get(0);
    assert!(id > 0, "generate_id() must return a positive HeerId");

    // 4d. `djogi_schema_migrations` ledger row is `applied`.
    let status: String = client
        .query_one(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&PHASE_ZERO_VERSION],
        )
        .await
        .expect("ledger row query")
        .get(0);
    assert_eq!(status, "applied", "bootstrap migration ledger row must be `applied`");

    drop(client);
    let _ = driver.await;

    // 5. Cleanup: drop the virgin DB.
    let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
        .await
        .expect("admin teardown connect");
    let teardown_driver = tokio::spawn(async move {
        if let Err(e) = admin_conn.await {
            eprintln!("[bootstrap replay] teardown driver: {e}");
        }
    });
    let _ = admin_client
        .batch_execute(&format!(
            "DROP DATABASE IF EXISTS \"{virgin_db}\" WITH (FORCE)"
        ))
        .await;
    drop(admin_client);
    let _ = teardown_driver.await;

    safe_remove_workspace(&work);
}

/// Proves the `#275` checksum-parity gate is checked before the
/// destructive `DROP DATABASE`: once a migration has been applied and
/// recorded in the live ledger, editing its on-disk SQL must refuse a
/// later `db reset` while leaving the existing database untouched.
#[tokio::test]
async fn db_reset_refuses_checksum_drift_before_drop() {
    let admin_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL env var must be set for live integration tests");

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let virgin_db = format!("djogi_drift_{stamp}");

    {
        let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
            .await
            .expect("admin connect");
        let admin_driver = tokio::spawn(async move {
            if let Err(e) = admin_conn.await {
                eprintln!("[bootstrap drift] admin driver: {e}");
            }
        });
        admin_client
            .batch_execute(&format!("CREATE DATABASE \"{virgin_db}\""))
            .await
            .expect("CREATE virgin DATABASE");
        drop(admin_client);
        let _ = admin_driver.await;
    }

    let work = temp_workspace("db_reset_drift");
    let guard = lock_for(&work);
    let bucket = BucketKey {
        database: virgin_db.clone(),
        app: String::new(),
    };
    let mut models = BTreeMap::new();
    models.insert(bucket.clone(), empty_schema_for(&bucket));
    let mut snapshots = BTreeMap::new();
    snapshots.insert(bucket.clone(), empty_schema_for(&bucket));
    let apps = vec![AppLifecycle {
        label: String::new(),
        database: virgin_db.clone(),
        renamed_from: None,
        tombstone: false,
    }];

    let compose_req = ComposeRequest {
        workspace_root: &work,
        models: &models,
        snapshots: &snapshots,
        apps: &apps,
        name: "phase_zero_drift_test",
        allow_destructive: false,
        force_overwrite: false,
        now: at(2026, 5, 4, 12, 30, 0),
        _guard: &guard,
        pk_flip_join_table_option: None,
        skip_phase_zero_auto_emit: false,
    };
    let compose_report = compose(compose_req).expect("compose with auto-emit");
    assert_eq!(compose_report.emitted_phase_zero.len(), 1);
    drop(guard);

    let virgin_url = replace_db_in_url(&admin_url, &virgin_db);
    let reset_req = ResetRequest {
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
    let first_reset = reset_app_database(reset_req)
        .await
        .expect("initial reset must succeed");
    assert!(
        first_reset
            .replayed_versions
            .iter()
            .any(|v| v.version == PHASE_ZERO_VERSION)
    );

    let (client, conn) = tokio_postgres::connect(&virgin_url, NoTls)
        .await
        .expect("connect to bootstrapped DB");
    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("[bootstrap drift] post-reset driver: {e}");
        }
    });

    let original_checksum: String = client
        .query_one(
            "SELECT checksum_up FROM djogi_schema_migrations WHERE version = $1",
            &[&PHASE_ZERO_VERSION],
        )
        .await
        .expect("read original ledger checksum")
        .get(0);

    drop(client);
    let _ = driver.await;

    let phase_zero_path = work.join(format!(
        "migrations/{virgin_db}/_global_/{PHASE_ZERO_VERSION}.sdjql"
    ));
    let work_canon = std::fs::canonicalize(&work).expect("canonicalize workspace");
    if !phase_zero_path.canonicalize().unwrap_or(phase_zero_path.clone()).starts_with(&work_canon) {
        panic!("phase_zero_path escapes workspace");
    }
    let original_sql = fs::read_to_string(&phase_zero_path).expect("read bootstrap SQL");
    fs::write(&phase_zero_path, format!("{original_sql}\n-- checksum drift for #275\n"))
        .expect("mutate bootstrap SQL");

    let err = reset_app_database(ResetRequest {
        database_url: &virgin_url,
        maintenance_database: "postgres",
        workspace_root: &work,
        profile: "test",
        confirmed: true,
        allow_checksum_drift_reset: false,
        migrate_config: MigrateConfig::default(),
        audit_pool: None,
        runner_identity: Some(RunnerIdentity::SingleNodeDev),
    })
    .await
    .expect_err("drifted file must refuse before destructive reset");

    match err {
        ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
            let phase_zero_issue = issues
                .iter()
                .find(|issue| issue.version == PHASE_ZERO_VERSION)
                .expect("bootstrap drift issue must be reported");
            assert_eq!(phase_zero_issue.bucket.database, virgin_db);
            assert_eq!(phase_zero_issue.bucket.app, "");
            assert_eq!(phase_zero_issue.sql_side, ResetSqlSide::Up);
            assert_eq!(phase_zero_issue.ledger_checksum, original_checksum);
            assert!(
                phase_zero_issue.on_disk_checksum.is_some(),
                "drift issue must name the on-disk checksum"
            );
        }
        other => panic!("expected checksum-parity refusal, got {other:?}"),
    }

    let (client, conn) = tokio_postgres::connect(&virgin_url, NoTls)
        .await
        .expect("reconnect after refusal");
    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("[bootstrap drift] refusal driver: {e}");
        }
    });

    let checksum_after_refusal: String = client
        .query_one(
            "SELECT checksum_up FROM djogi_schema_migrations WHERE version = $1",
            &[&PHASE_ZERO_VERSION],
        )
        .await
        .expect("read checksum after refusal")
        .get(0);
    assert_eq!(
        checksum_after_refusal, original_checksum,
        "ledger row must remain untouched when drift refusal fires before DROP DATABASE"
    );

    let heer_nodes_exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = 'heer_nodes' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("heer_nodes existence query")
        .get(0);
    assert!(
        heer_nodes_exists,
        "existing database state must remain intact when drift refusal fires"
    );

    drop(client);
    let _ = driver.await;

    let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
        .await
        .expect("admin teardown connect");
    let teardown_driver = tokio::spawn(async move {
        if let Err(e) = admin_conn.await {
            eprintln!("[bootstrap drift] teardown driver: {e}");
        }
    });
    let _ = admin_client
        .batch_execute(&format!(
            "DROP DATABASE IF EXISTS \"{virgin_db}\" WITH (FORCE)"
        ))
        .await;
    drop(admin_client);
    let _ = teardown_driver.await;

    safe_remove_workspace(&work);
}
