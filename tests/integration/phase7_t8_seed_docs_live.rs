//! Phase 7 T8 — live-PG integration tests for the seed runner +
//! deterministic docs generation.
//!
//! # What these tests prove
//!
//! Seed runner:
//! - Discovers `*.sql` files under `seeds/<database>/`, runs each in
//!   alphabetical order, and records every successful application
//!   in `djogi_seed_runs`.
//! - Re-running the runner skips seeds whose `V1:<sha256>` checksum
//!   matches the recorded value (idempotency).
//! - A hand-edited seed (checksum drift vs. ledger) refuses with
//!   [`SeedError::ChecksumDrift`] before re-applying — operator must
//!   either revert or delete the ledger row.
//! - The localhost gate refuses non-localhost DATABASE_URL when
//!   `allow_non_localhost = false`; an explicit override unblocks.
//!
//! Docs generation:
//! - `generate_docs` produces a deterministic README + per-app
//!   directories. Two consecutive renders of the same inventory write
//!   byte-identical output.
//! - Empty inventory still emits a sentinel README (not an error).
//!
//! `db reset`'s triple gate is unit-tested in `djogi/src/migrate/reset.rs`
//! itself — exercising the live drop/create path here would require
//! the test harness to give up its own database lifecycle, which
//! conflicts with `#[djogi_test]`. The gate logic is the load-bearing
//! part of `db reset`; the post-gate path is a thin wrapper around
//! the already-tested `apply_plan`.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;

use djogi::migrate::{SeedError, SeedOutcome, run_seeds};

// ── Helpers ───────────────────────────────────────────────────────────────

fn temp_workspace(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("djogi-t8-{tag}-{stamp}-{n}"));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Resolve the connected test context's `current_database()` so the
/// seed walker scans `seeds/<that>/`.
async fn current_database(ctx: &mut djogi::DjogiContext) -> String {
    ctx.raw_scalar::<String>("SELECT current_database()::text", &[])
        .await
        .expect("current_database")
}

// ── Seed runner: happy path + idempotency ─────────────────────────────────

#[djogi::djogi_test]
async fn seed_runner_applies_seeds_and_records_in_ledger(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("seed_apply");
    let database = current_database(&mut ctx).await;
    let seeds_dir = work.join("seeds").join(&database);
    fs::create_dir_all(&seeds_dir).unwrap();

    // Two SQL seed files. The seeds create + populate a small
    // reference table; we cleanly assert post-state.
    fs::write(
        seeds_dir.join("01_init.sql"),
        "CREATE TABLE t8_seed_widgets (id BIGINT PRIMARY KEY, name TEXT NOT NULL);\n\
         INSERT INTO t8_seed_widgets (id, name) VALUES (1, 'alpha');\n",
    )
    .unwrap();
    fs::write(
        seeds_dir.join("02_data.sql"),
        "INSERT INTO t8_seed_widgets (id, name) VALUES (2, 'beta');\n",
    )
    .unwrap();

    // First run — both seeds must apply.
    let report = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect("first run ok");
    let outcomes: Vec<SeedOutcome> = report.entries.iter().map(|e| e.outcome).collect();
    assert_eq!(outcomes, vec![SeedOutcome::Applied, SeedOutcome::Applied]);

    // The data must have landed.
    let count: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM t8_seed_widgets", &[])
        .await
        .expect("count");
    assert_eq!(count, 2);

    // The ledger must have one row per seed, with the recorded names
    // matching the file stems. We project a synthetic concatenation
    // so the raw_scalar lookup returns one value across both rows
    // joined by `,` — that side-steps the "no FromPgRow for one-tuple"
    // gap without needing a hand-rolled FromPgRow for this test.
    let joined: String = ctx
        .raw_scalar::<String>(
            "SELECT string_agg(seed_name, ',' ORDER BY seed_name) \
             FROM djogi_seed_runs",
            &[],
        )
        .await
        .expect("ledger query");
    assert_eq!(joined, "01_init,02_data");

    // Second run — both seeds must skip (already applied).
    let report = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect("second run ok");
    let outcomes: Vec<SeedOutcome> = report.entries.iter().map(|e| e.outcome).collect();
    assert_eq!(
        outcomes,
        vec![
            SeedOutcome::SkippedAlreadyApplied,
            SeedOutcome::SkippedAlreadyApplied,
        ]
    );

    // Re-running must not have inserted duplicate rows or re-run any
    // INSERT (the unique key on the table would have surfaced as an
    // error).
    let count: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM t8_seed_widgets", &[])
        .await
        .expect("count");
    assert_eq!(count, 2, "no duplicate INSERTs after second run");

    // Cleanup the workspace; the test DB itself is dropped by the
    // harness on return.
    let _ = fs::remove_dir_all(&work);
}

// ── Seed runner: checksum drift refusal ───────────────────────────────────

#[djogi::djogi_test]
async fn seed_runner_refuses_on_checksum_drift(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("seed_drift");
    let database = current_database(&mut ctx).await;
    let seeds_dir = work.join("seeds").join(&database);
    fs::create_dir_all(&seeds_dir).unwrap();

    // First run — apply a seed.
    let seed_path = seeds_dir.join("01_init.sql");
    fs::write(
        &seed_path,
        "CREATE TABLE t8_seed_drift (id BIGINT PRIMARY KEY);\n\
         INSERT INTO t8_seed_drift (id) VALUES (1);\n",
    )
    .unwrap();
    run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect("initial apply");

    // Mutate the seed file on disk — anything that changes the
    // bytes flips the checksum.
    fs::write(
        &seed_path,
        "CREATE TABLE t8_seed_drift (id BIGINT PRIMARY KEY);\n\
         INSERT INTO t8_seed_drift (id) VALUES (1);\n\
         -- drift comment\n",
    )
    .unwrap();

    // Second run — must refuse.
    let err = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("must refuse on drift");
    match err {
        SeedError::ChecksumDrift {
            seed_name,
            ledger_checksum,
            on_disk_checksum,
        } => {
            assert_eq!(seed_name, "01_init");
            assert_ne!(ledger_checksum, on_disk_checksum);
            assert!(ledger_checksum.starts_with("V1:"));
            assert!(on_disk_checksum.starts_with("V1:"));
        }
        other => panic!("expected ChecksumDrift, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&work);
}

// ── Seed runner: localhost gate ───────────────────────────────────────────

#[djogi::djogi_test]
async fn seed_runner_refuses_remote_url_without_override(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("seed_remote");
    let database = current_database(&mut ctx).await;

    // No seeds need to exist — the gate fires before the directory
    // walk.
    let err = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://prod.example.com:5432/main",
        false, // allow_non_localhost
    )
    .await
    .expect_err("must refuse");
    match err {
        SeedError::LocalhostGate { database_url } => {
            assert_eq!(database_url, "postgres://prod.example.com:5432/main");
        }
        other => panic!("expected LocalhostGate, got {other:?}"),
    }

    // With the override flag set, the gate steps aside — the runner
    // bootstraps the ledger and exits with an empty report (no
    // seeds discovered).
    let report = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://prod.example.com:5432/main",
        true, // allow_non_localhost
    )
    .await
    .expect("override must allow run to proceed");
    assert!(report.entries.is_empty(), "no seeds present in workspace");
    let _ = fs::remove_dir_all(&work);
}

// ── Docs: live test against an empty inventory ────────────────────────────
//
// The deterministic + non-empty inventory rendering paths are unit
// tested in `djogi::migrate::docs::tests`; this live test confirms
// the public `generate_docs` entry compiles and writes the expected
// scaffolding when invoked from an integration test (where
// `inventory::iter::<ModelDescriptor>` may carry the framework's
// own internal fixtures).

#[djogi::djogi_test]
async fn docs_generate_produces_readme_under_arbitrary_root(mut _ctx: djogi::DjogiContext) {
    // We don't even need the DB context for docs — the renderer
    // operates on the in-process descriptor inventory only. The
    // `mut _ctx` parameter is kept so the test composes the same way
    // every other live test does (the harness owns ctx lifetime).
    let work = temp_workspace("docs_run");
    let out = work.join("target/djogi-docs");
    let report = djogi::migrate::generate_docs(&out).expect("generate docs");
    // Either the inventory is empty (no `#[model]`-generated
    // descriptors in the integration binary) or it is not — both
    // shapes must produce a README.
    let readme = std::fs::read_to_string(out.join("README.md")).expect("README must exist");
    assert!(readme.contains("Djogi model reference"));
    if report.models_rendered == 0 {
        assert!(readme.contains("No models registered"));
    }
    let _ = fs::remove_dir_all(&work);
}
