//! Phase 8-Zero Track 0 (sub-step 0.3) — `migrations compose` auto-emits
//! a Phase 0 bootstrap migration for any database that doesn't already
//! have one.
//!
//! # What this proves
//!
//! - `compose` writes the Phase 0 up SQL, down SQL, and pending JSON
//!   files at the canonical
//!   `migrations/<db>/_global_/V00000000000000__phase_zero_bootstrap.sql`
//!   path on first invocation.
//! - The composed up SQL contains the HeeRanjID install, every
//!   declared extension's `CREATE EXTENSION IF NOT EXISTS`, and the
//!   node-id GUC seed.
//! - A second compose call (Phase 0 already on disk) does not re-emit
//!   the file and leaves the on-disk bytes byte-for-byte unchanged.
//! - Extension declarations from the descriptor inventory propagate
//!   into Phase 0's `CREATE EXTENSION` block — verified by injecting
//!   a synthetic `IndexSchema` with `extension_dependency = Some("postgis")`
//!   into the compose's `models` map.
//! - Compose returns a `Vec<EmittedPhaseZero>` in `report.emitted_phase_zero`
//!   that names every emission, with `extensions` populated.
//!
//! # Why a separate integration test
//!
//! The unit tests in `bootstrap::tests` cover the pure composition
//! functions and the in-isolation `ensure_phase_zero_emitted` driver.
//! This integration test exercises the full `compose()` entry point —
//! including the wiring between `compose`'s pre-flight call to
//! `ensure_phase_zero_emitted` and the rest of the regular delta-based
//! work. It catches regressions where `compose` could skip the
//! auto-emit (e.g. a future refactor that gates on `models.is_empty()`).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use djogi::migrate::{
    AppLifecycle, AppliedSchema, BucketKey, ComposeRequest, GUARD_DEFAULT_TIMEOUT, IndexKindSchema,
    IndexSchema, IndexTargetSchema, IndexTypeSchema, LOCK_FILE_NAME, PHASE_ZERO_VERSION,
    SNAPSHOT_FORMAT_VERSION, WorkspaceGuard, acquire_workspace_lock, compose,
};

// ── Helpers ───────────────────────────────────────────────────────────────

fn temp_workspace(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("djogi-phase0-emit-{label}-{stamp}"));
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
        generated_at: "2026-05-04T00:00:00Z".to_string(),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: vec![bucket.app.clone()],
    }
}

fn schema_with_postgis_index(bucket: &BucketKey) -> AppliedSchema {
    let mut schema = empty_schema_for(bucket);
    schema.indexes.push(IndexSchema {
        extension_dependency: Some("postgis".to_string()),
        include: Vec::new(),
        index_type: IndexTypeSchema::BTree,
        kind: IndexKindSchema::NonUnique,
        name: "synthetic_geom_idx".to_string(),
        nulls_not_distinct: false,
        predicate: None,
        requires_out_of_transaction: false,
        table: "synthetic_table".to_string(),
        target: IndexTargetSchema::Columns(Vec::new()),
    });
    schema
}

fn at(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> time::OffsetDateTime {
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day).unwrap();
    let t = time::Time::from_hms(hour, minute, second).unwrap();
    date.with_time(t).assume_utc()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[test]
fn compose_auto_emits_phase_zero_with_postgis_dependency_on_first_run() {
    let work = temp_workspace("auto_emit_postgis");
    let guard = lock_for(&work);

    // Set up the inputs: one bucket on database "main" with a
    // synthetic stand-alone index whose `extension_dependency =
    // Some("postgis")`. The auto-emit walks `AppliedSchema::indexes`
    // (the per-bucket flat index list) — no need to construct a
    // full table fixture.
    //
    // We exercise `ensure_phase_zero_emitted` directly here rather
    // than the full `compose()` entry point because the differ would
    // refuse the synthetic index (no parent table); the wiring from
    // `compose()` to `ensure_phase_zero_emitted` is exercised by
    // `compose_auto_emit_returns_emissions_in_report` below.
    let bucket = BucketKey {
        database: "main".to_string(),
        app: String::new(),
    };
    let mut models = BTreeMap::new();
    models.insert(bucket.clone(), schema_with_postgis_index(&bucket));
    let apps = vec![AppLifecycle {
        label: String::new(),
        database: "main".to_string(),
        renamed_from: None,
        tombstone: false,
    }];

    let emitted = djogi::migrate::ensure_phase_zero_emitted(
        &work,
        &models,
        &apps,
        at(2026, 5, 4, 12, 0, 0),
        &guard,
    )
    .expect("emit");

    assert_eq!(emitted.len(), 1, "one Phase 0 per database");
    assert_eq!(emitted[0].database, "main");
    assert!(
        emitted[0].extensions.contains("postgis"),
        "PostGIS dependency must propagate into Phase 0"
    );

    // On disk: SQL pair + pending JSON at the canonical paths.
    let phase_zero_dir = work.join("migrations").join("main").join("_global_");
    let up_path = phase_zero_dir.join(format!("{PHASE_ZERO_VERSION}.sql"));
    let down_path = phase_zero_dir.join(format!("{PHASE_ZERO_VERSION}.down.sql"));
    let pending_path = work
        .join("target")
        .join("djogi_pending")
        .join("main")
        .join("_global_.json");
    assert!(
        up_path.exists(),
        "up SQL must exist at {}",
        up_path.display()
    );
    assert!(down_path.exists(), "down SQL must exist");
    assert!(pending_path.exists(), "pending JSON must exist");

    // Up SQL inspection: HeeRanjID install + PostGIS extension + ALTER DATABASE.
    let up_sql = fs::read_to_string(&up_path).expect("read up");
    assert!(
        up_sql.contains("HeeRanjID base schema"),
        "up SQL must include HeeRanjID install"
    );
    assert!(
        up_sql.contains("CREATE EXTENSION IF NOT EXISTS \"postgis\""),
        "up SQL must include PostGIS install (got: {up_sql})"
    );
    assert!(
        up_sql.contains("ALTER DATABASE \"main\" SET heer.node_id = '1'"),
        "up SQL must include node-id GUC seed"
    );

    // Down SQL is comment-only.
    let down_sql = fs::read_to_string(&down_path).expect("read down");
    assert!(
        down_sql.contains("Phase 0 bootstrap — down"),
        "down SQL must carry the no-op marker"
    );
    assert!(
        !down_sql.contains("DROP TABLE") && !down_sql.contains("DROP EXTENSION"),
        "down SQL must not contain real DDL"
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn compose_auto_emit_is_idempotent_across_runs() {
    let work = temp_workspace("auto_emit_idempotent");
    let guard = lock_for(&work);

    let apps = vec![AppLifecycle {
        label: String::new(),
        database: "main".to_string(),
        renamed_from: None,
        tombstone: false,
    }];
    let models = BTreeMap::new();

    let first = djogi::migrate::ensure_phase_zero_emitted(
        &work,
        &models,
        &apps,
        at(2026, 5, 4, 12, 0, 0),
        &guard,
    )
    .expect("first emit");
    assert_eq!(first.len(), 1);
    let first_up = fs::read(&first[0].up_sql_path).expect("read first up");

    let second = djogi::migrate::ensure_phase_zero_emitted(
        &work,
        &models,
        &apps,
        at(2026, 5, 4, 12, 0, 1),
        &guard,
    )
    .expect("second emit");
    assert!(
        second.is_empty(),
        "second compose must NOT re-emit Phase 0 (got: {second:?})"
    );

    // Bytes on disk are unchanged.
    let first_up_after = fs::read(&first[0].up_sql_path).expect("read after second");
    assert_eq!(first_up, first_up_after);

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn compose_auto_emit_returns_emissions_in_report() {
    // End-to-end through `compose()` with empty models + snapshots
    // for a registered app. The regular delta path returns
    // NothingToCompose since no tables changed; the auto-emit path
    // pre-emits Phase 0 for the database; compose surfaces this as
    // a successful report (NOT NothingToCompose) per Track 0
    // semantics.
    let work = temp_workspace("compose_returns_report");
    let guard = lock_for(&work);

    let bucket = BucketKey {
        database: "main".to_string(),
        app: String::new(),
    };
    let mut snapshots = BTreeMap::new();
    snapshots.insert(bucket.clone(), empty_schema_for(&bucket));
    let mut models = BTreeMap::new();
    models.insert(bucket.clone(), empty_schema_for(&bucket));
    let apps = vec![AppLifecycle {
        label: String::new(),
        database: "main".to_string(),
        renamed_from: None,
        tombstone: false,
    }];

    let req = ComposeRequest {
        workspace_root: &work,
        models: &models,
        snapshots: &snapshots,
        apps: &apps,
        name: "phase_zero_emit_test",
        allow_destructive: false,
        force_overwrite: false,
        now: at(2026, 5, 4, 12, 0, 0),
        _guard: &guard,
        pk_flip_join_table_option: None,
        skip_phase_zero_auto_emit: false,
    };

    let report = compose(req).expect("compose with auto-emit must succeed");
    assert_eq!(report.composed_buckets.len(), 0, "no delta to compose");
    assert_eq!(
        report.emitted_phase_zero.len(),
        1,
        "Phase 0 must be auto-emitted"
    );
    assert_eq!(report.emitted_phase_zero[0].database, "main");

    // Run compose again — Phase 0 already on disk, no delta changes
    // → NothingToCompose. (skipping the destructure for clarity.)
    let req2 = ComposeRequest {
        workspace_root: &work,
        models: &models,
        snapshots: &snapshots,
        apps: &apps,
        name: "phase_zero_emit_test2",
        allow_destructive: false,
        force_overwrite: false,
        now: at(2026, 5, 4, 12, 0, 1),
        _guard: &guard,
        pk_flip_join_table_option: None,
        skip_phase_zero_auto_emit: false,
    };
    let err = compose(req2).expect_err("second compose should be NothingToCompose");
    assert!(matches!(
        err,
        djogi::migrate::ComposeError::NothingToCompose
    ));

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn compose_auto_emit_aggregates_extensions_across_apps_in_same_database() {
    let work = temp_workspace("auto_emit_aggregate");
    let guard = lock_for(&work);

    let billing_bucket = BucketKey {
        database: "main".to_string(),
        app: "billing".to_string(),
    };
    let shipping_bucket = BucketKey {
        database: "main".to_string(),
        app: "shipping".to_string(),
    };
    let mut models = BTreeMap::new();
    models.insert(
        billing_bucket.clone(),
        schema_with_postgis_index(&billing_bucket),
    );
    let mut shipping_schema = empty_schema_for(&shipping_bucket);
    shipping_schema.indexes.push(IndexSchema {
        extension_dependency: Some("pg_trgm".to_string()),
        include: Vec::new(),
        index_type: IndexTypeSchema::BTree,
        kind: IndexKindSchema::NonUnique,
        name: "synthetic_text_idx".to_string(),
        nulls_not_distinct: false,
        predicate: None,
        requires_out_of_transaction: false,
        table: "synthetic_table".to_string(),
        target: IndexTargetSchema::Columns(Vec::new()),
    });
    models.insert(shipping_bucket.clone(), shipping_schema);

    let apps = vec![
        AppLifecycle {
            label: "billing".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        },
        AppLifecycle {
            label: "shipping".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        },
    ];

    let emitted = djogi::migrate::ensure_phase_zero_emitted(
        &work,
        &models,
        &apps,
        at(2026, 5, 4, 12, 0, 0),
        &guard,
    )
    .expect("emit");

    assert_eq!(emitted.len(), 1, "one Phase 0 per database (main)");
    let main_emit = &emitted[0];
    assert_eq!(main_emit.database, "main");
    assert!(
        main_emit.extensions.contains("postgis"),
        "billing's PostGIS dep must aggregate into main's Phase 0"
    );
    assert!(
        main_emit.extensions.contains("pg_trgm"),
        "shipping's pg_trgm dep must aggregate into main's Phase 0"
    );
    assert_eq!(main_emit.extensions.len(), 2, "no spurious extensions");

    let _ = fs::remove_dir_all(&work);
}
