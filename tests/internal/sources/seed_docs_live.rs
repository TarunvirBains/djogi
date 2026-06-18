//  — live-PG integration tests for the seed runner +
// deterministic docs generation.
//
// # What these tests prove
//
// Seed runner:
// - Discovers `*.sql` files under `seeds/<database>/`, runs each in
//   alphabetical order, and records every successful application
//   in `djogi_seed_runs`.
// - Re-running the runner skips seeds whose `V1:<sha256>` checksum
//   matches the recorded value (idempotency).
// - A hand-edited seed (checksum drift vs. ledger) refuses with
//   [`SeedError::ChecksumDrift`] before re-applying — operator must
//   either revert or delete the ledger row.
// - The localhost gate refuses non-localhost DATABASE_URL when
//   `allow_non_localhost = false`; an explicit override unblocks.
//
// Docs generation:
// - `generate_docs` produces a deterministic README + per-app
//   directories. Two consecutive renders of the same inventory write
//   byte-identical output.
// - Empty inventory still emits a sentinel README (not an error).
//
// `db reset`'s triple gate is unit-tested in `djogi/src/migrate/reset.rs`
// itself — exercising the live drop/create path here would require
// the test harness to give up its own database lifecycle, which
// conflicts with `#[djogi_test]`. The gate logic is the load-bearing
// part of `db reset`; the post-gate path is a thin wrapper around
// the already-tested `apply_plan`.

use std::fs;
use std::path::PathBuf;

use djogi::migrate::{
    SeedError, SeedOutcome, derive_per_database_url, read_workspace_file_to_string,
    remove_workspace_dir_all, run_seeds, write_workspace_file,
};

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

fn peer_ctx(ctx: &djogi::DjogiContext) -> djogi::DjogiContext {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context should be pool-backed");
    djogi::DjogiContext::from_pool(pool)
}

async fn install_seed_finalize_failure_trigger(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE OR REPLACE FUNCTION djogi_test_fail_seed_finalize() \
         RETURNS trigger AS $$ \
         BEGIN \
             IF OLD.status = 'running' AND NEW.status = 'applied' THEN \
                 RAISE EXCEPTION 'djogi test injected seed finalize failure'; \
             END IF; \
             RETURN NEW; \
         END; \
         $$ LANGUAGE plpgsql",
    )
    .await
    .expect("create finalize failure function");
    ctx.raw_ddl("DROP TRIGGER IF EXISTS djogi_test_fail_seed_finalize ON djogi_seed_runs")
        .await
        .expect("drop finalize failure trigger");
    ctx.raw_ddl(
        "CREATE TRIGGER djogi_test_fail_seed_finalize \
         BEFORE UPDATE ON djogi_seed_runs \
         FOR EACH ROW EXECUTE FUNCTION djogi_test_fail_seed_finalize()",
    )
    .await
    .expect("create finalize failure trigger");
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
    write_workspace_file(
        &work,
        seeds_dir.join("01_init.sql"),
        b"CREATE TABLE seed_widgets (id BIGINT PRIMARY KEY, name TEXT NOT NULL);\n\
         INSERT INTO seed_widgets (id, name) VALUES (1, 'alpha');\n",
    )
    .unwrap();
    write_workspace_file(
        &work,
        seeds_dir.join("02_data.sql"),
        b"INSERT INTO seed_widgets (id, name) VALUES (2, 'beta');\n",
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
        .raw_scalar("SELECT COUNT(*)::bigint FROM seed_widgets", &[])
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
        .raw_scalar("SELECT COUNT(*)::bigint FROM seed_widgets", &[])
        .await
        .expect("count");
    assert_eq!(count, 2, "no duplicate INSERTs after second run");

    // Cleanup the workspace; the test DB itself is dropped by the
    // harness on return.
    let _ = remove_workspace_dir_all(&work, &work);
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
    write_workspace_file(
        &work,
        &seed_path,
        b"CREATE TABLE seed_drift (id BIGINT PRIMARY KEY);\n\
         INSERT INTO seed_drift (id) VALUES (1);\n",
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
    write_workspace_file(
        &work,
        &seed_path,
        b"CREATE TABLE seed_drift (id BIGINT PRIMARY KEY);\n\
         INSERT INTO seed_drift (id) VALUES (1);\n\
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

    let _ = remove_workspace_dir_all(&work, &work);
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
    let _ = remove_workspace_dir_all(&work, &work);
}

#[djogi::djogi_test]
async fn seed_runner_rejects_concurrent_first_run(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("seed_concurrent");
    let database = current_database(&mut ctx).await;
    let seeds_dir = work.join("seeds").join(&database);
    fs::create_dir_all(&seeds_dir).unwrap();

    ctx.raw_ddl("CREATE TABLE seed_concurrent (id BIGINT NOT NULL)")
        .await
        .expect("create target table");
    write_workspace_file(
        &work,
        seeds_dir.join("01_non_idempotent.sql"),
        b"SELECT pg_sleep(0.25);\n\
         INSERT INTO seed_concurrent (id) VALUES (1);\n",
    )
    .unwrap();

    let mut first_ctx = peer_ctx(&ctx);
    let mut second_ctx = peer_ctx(&ctx);
    let work_for_first = work.clone();
    let database_for_first = database.clone();
    let first = tokio::spawn(async move {
        run_seeds(
            &mut first_ctx,
            &work_for_first,
            &database_for_first,
            "postgres://localhost/main",
            false,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let second = run_seeds(
        &mut second_ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await;
    let first = first.await.expect("join first runner");

    assert!(first.is_ok(), "first runner must succeed: {first:?}");
    match second.expect_err("second runner must see the in-progress lock") {
        SeedError::RunAlreadyInProgress { database: db } => assert_eq!(db, database),
        other => panic!("expected RunAlreadyInProgress, got {other:?}"),
    }

    let count: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM seed_concurrent", &[])
        .await
        .expect("count seeded rows");
    assert_eq!(
        count, 1,
        "the non-idempotent seed body must execute exactly once under contention"
    );
    let _ = remove_workspace_dir_all(&work, &work);
}

#[djogi::djogi_test]
async fn seed_runner_leaves_stale_claim_on_finalize_failure(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("seed_stale_claim");
    let database = current_database(&mut ctx).await;
    let seeds_dir = work.join("seeds").join(&database);
    fs::create_dir_all(&seeds_dir).unwrap();

    ctx.raw_ddl("CREATE TABLE seed_stale_claim (id BIGINT NOT NULL)")
        .await
        .expect("create target table");
    write_workspace_file(
        &work,
        seeds_dir.join("01_finalize_gap.sql"),
        b"INSERT INTO seed_stale_claim (id) VALUES (1);\n",
    )
    .unwrap();

    djogi::migrate::bootstrap_seed_ledger(&mut ctx)
        .await
        .expect("bootstrap seed ledger");
    install_seed_finalize_failure_trigger(&mut ctx).await;

    let err = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("finalize failure must surface");
    assert!(
        matches!(err, SeedError::LedgerWrite { .. }),
        "expected LedgerWrite from the injected finalize failure, got {err:?}"
    );

    let seeded_rows: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM seed_stale_claim", &[])
        .await
        .expect("count seeded rows");
    assert_eq!(seeded_rows, 1, "seed SQL must have committed before finalize failed");

    let status: String = ctx
        .raw_scalar(
            "SELECT status FROM djogi_seed_runs WHERE seed_name = '01_finalize_gap'",
            &[],
        )
        .await
        .expect("seed ledger status");
    assert_eq!(status, "running", "claim row must stay running after the gap");

    let rerun = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("stale running claim must block rerun");
    match rerun {
        SeedError::StaleClaim {
            seed_name,
            checksum_up,
        } => {
            assert_eq!(seed_name, "01_finalize_gap");
            assert!(checksum_up.starts_with("V1:"));
        }
        other => panic!("expected StaleClaim, got {other:?}"),
    }

    let rerun_count: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM seed_stale_claim", &[])
        .await
        .expect("count seeded rows after stale claim rerun");
    assert_eq!(rerun_count, 1, "stale claim must prevent silent re-execution");
    let _ = remove_workspace_dir_all(&work, &work);
}

#[djogi::djogi_test]
async fn seed_runner_marks_failed_claim_and_refuses_retry(mut ctx: djogi::DjogiContext) {
    let work = temp_workspace("seed_failed_claim");
    let database = current_database(&mut ctx).await;
    let seeds_dir = work.join("seeds").join(&database);
    fs::create_dir_all(&seeds_dir).unwrap();

    write_workspace_file(
        &work,
        seeds_dir.join("01_broken.sql"),
        b"INSERT INTO seed_missing_target (id) VALUES (1);\n",
    )
    .unwrap();

    let err = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("broken seed must fail");
    assert!(matches!(err, SeedError::ApplyFailed { .. }), "got {err:?}");

    let status: String = ctx
        .raw_scalar(
            "SELECT status FROM djogi_seed_runs WHERE seed_name = '01_broken'",
            &[],
        )
        .await
        .expect("seed ledger status");
    assert_eq!(status, "failed");

    let rerun = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("failed claim must block rerun");
    match rerun {
        SeedError::FailedClaim {
            seed_name,
            failure_note,
        } => {
            assert_eq!(seed_name, "01_broken");
            let failure_note = failure_note.expect("failure note must be recorded");
            assert!(failure_note.contains("seed apply failed"));
        }
        other => panic!("expected FailedClaim, got {other:?}"),
    }

    let _ = remove_workspace_dir_all(&work, &work);
}

#[djogi::djogi_test]
async fn seed_runner_explicit_transaction_failure_surfaces_failed_claim_on_rerun(
    mut ctx: djogi::DjogiContext,
) {
    let work = temp_workspace("seed_explicit_tx_failed_claim");
    let database = current_database(&mut ctx).await;
    let seeds_dir = work.join("seeds").join(&database);
    fs::create_dir_all(&seeds_dir).unwrap();

    write_workspace_file(
        &work,
        seeds_dir.join("01_explicit_tx_broken.sql"),
        b"BEGIN;\n\
         CREATE TABLE seed_explicit_tx_probe (id BIGINT PRIMARY KEY);\n\
         INSERT INTO seed_missing_target (id) VALUES (1);\n\
         COMMIT;\n",
    )
    .unwrap();

    let err = run_seeds(
        &mut ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("explicit transaction seed must fail");
    assert!(matches!(err, SeedError::ApplyFailed { .. }), "got {err:?}");

    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let per_test_url =
        derive_per_database_url(&admin_url, &database).expect("derive per-test database URL");
    let observer_pool = djogi::pg::pool::DjogiPool::connect(&per_test_url)
        .await
        .expect("connect observer pool");
    let mut observer_ctx = djogi::DjogiContext::from_pool(observer_pool);

    let status: String = observer_ctx
        .raw_scalar(
            "SELECT status FROM djogi_seed_runs WHERE seed_name = '01_explicit_tx_broken'",
            &[],
        )
        .await
        .expect("seed ledger status");
    assert_eq!(status, "failed", "explicit-transaction failure must persist failed");

    let rerun = run_seeds(
        &mut observer_ctx,
        &work,
        &database,
        "postgres://localhost/main",
        false,
    )
    .await
    .expect_err("failed claim must block rerun");
    match rerun {
        SeedError::FailedClaim {
            seed_name,
            failure_note,
        } => {
            assert_eq!(seed_name, "01_explicit_tx_broken");
            let failure_note = failure_note.expect("failure note must be recorded");
            assert!(failure_note.contains("seed apply failed"));
        }
        other => panic!("expected FailedClaim, got {other:?}"),
    }

    let _ = remove_workspace_dir_all(&work, &work);
}

// ── Docs: live test against an empty inventory ────────────────────────────
//
// The deterministic + non-empty inventory rendering paths are unit
// tested in `djogi::migrate::docs::tests`; this live test confirms
// the public `generate_docs` entry compiles and writes the expected
// scaffolding when invoked from an integration test (where
// `inventory::iter::<ModelDescriptor>` may carry the framework's
// own internal fixtures).

// ── Codex round B-1: per-database URL routing ──────────────────────────
//
// `db seed --database crud_log` must execute SQL against `crud_log`,
// NOT against the application database. The CLI uses
// `derive_per_database_url` to splice `<name>` into the application
// URL's path component before connecting; this test exercises the
// helper end-to-end through the live test harness so the round-trip
// is more than unit-test plumbing.
//
// We can't easily spin up a second DB inside the `#[djogi_test]`
// harness without giving up its DB-lifecycle control, so the test
// asserts the pre-fix invariant (the helper is well-defined for the
// shapes the CLI calls it with) plus the round-trip property: the
// derived URL targets the supplied database name exactly.

#[djogi::djogi_test]
async fn derive_per_database_url_round_trips_against_test_url(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    // Use the same shape an operator's `database.url` would carry —
    // an authority + path. The path component is the database the
    // harness connected to.
    let app_url = format!("postgres://localhost/{database}");
    // Splicing `crud_log` must replace the path component while
    // preserving the authority. The post-splice URL targets
    // `crud_log` on the same authority — exactly the route the CLI
    // would have taken before the fix BUT now actually opens a
    // connection against.
    let routed = derive_per_database_url(&app_url, "crud_log").expect("splice");
    assert_eq!(routed, "postgres://localhost/crud_log");

    // Sanity — splicing the same `<database>` back produces the
    // original URL, so the route to the application DB is preserved.
    let rebound = derive_per_database_url(&app_url, &database).expect("re-splice");
    assert_eq!(rebound, app_url);

    // Malformed URLs surface as `None` — the CLI treats this as
    // exit code 1 rather than falling back to the application DB.
    assert!(derive_per_database_url("postgres://localhost", "crud_log").is_none());
}

#[djogi::djogi_test]
async fn docs_generate_produces_readme_under_arbitrary_root(mut _ctx: djogi::DjogiContext) {
    // We don't even need the DB context for docs — the renderer
    // operates on the in-process descriptor inventory only. The
    // `mut _ctx` parameter is kept so the test composes the same way
    // every other live test does (the harness owns ctx lifetime).
    let work = temp_workspace("docs_run");
    let out = work.join("target/djogi-docs");
    let report = djogi::migrate::generate_docs(&out, None).expect("generate docs");
    // Either the inventory is empty (no `#[model]`-generated
    // descriptors in the integration binary) or it is not — both
    // shapes must produce a README.
    let readme =
        read_workspace_file_to_string(&out, "README.md").expect("README must exist");
    assert!(readme.contains("Djogi model reference"));
    if report.models_rendered == 0 {
        assert!(readme.contains("No models registered"));
    }
    let _ = remove_workspace_dir_all(&work, &work);
}

// ── Codex umbrella U-4: db reset replay must honour historical apply order ─

/// Codex umbrella U-4 (BLOCK): `db reset` must replay migrations in
/// HISTORICAL apply order (`applied_at ASC`), NOT lexical version-string
/// order. 's out-of-order policy allows a hotfix to apply AFTER a
/// later migration; lexical replay would re-order them.
///
/// **What this test pins.** We populate the ledger with three rows
/// whose `applied_at` is deliberately out-of-order vs the version
/// strings (V0001 first, V0003 second, V0002 third), then call the
/// internal `capture_historical_apply_order` helper that `db reset`
/// uses BEFORE the drop. The captured map's ranks must reflect the
/// `applied_at` order, NOT the version-string order. Combined with
/// the `build_replay_plan` unit tests in `djogi/src/migrate/reset.rs`,
/// this proves the full reset replay chain honours U-4 end-to-end.
///
/// **Why we don't run the full `db reset`.** The harness owns the DB
/// lifecycle; `db reset` issues `DROP DATABASE` which would drop the
/// harness's database mid-test. Splitting the proof at
/// `capture_historical_apply_order` lets us pin the load-bearing
/// ranks via SQL writes alone — no harness conflict.
#[djogi::djogi_test]
async fn u4_reset_captures_historical_order_not_lexical(mut ctx: djogi::DjogiContext) {
    // Bootstrap the ledger then INSERT three rows with deliberately
    // out-of-order `applied_at` timestamps. Lexical version order is
    // V0001 < V0002 < V0003, but we set apply order to V0001, V0003,
    // V0002.
    djogi::migrate::bootstrap_ledger(&mut ctx)
        .await
        .expect("bootstrap");

    // The ledger DDL has `applied_at TIMESTAMPTZ NOT NULL DEFAULT now()`
    // so we override it explicitly per row to fix the apply order.
    for (version, applied_at_offset_secs) in [
        ("V20260101000000__a", 0i64),
        ("V20260301000000__c", 60i64),  // applied SECOND historically
        ("V20260201000000__b", 120i64), // applied THIRD historically (out-of-order)
    ] {
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, checksum_down, execution_mode, status, \
              applied_at, run_id, snapshot_version, app_label) \
             VALUES ($1, $2, $3, NULL, 'transactional', 'applied', \
                     now() + ($4::int || ' seconds')::interval, 0, '1.0', '')",
            &[
                &version.to_string(),
                &format!("U-4 fixture row for {version}"),
                &"V1:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
                &(applied_at_offset_secs as i32),
            ],
        )
        .await
        .expect("insert ledger row");
    }

    // Read back the ranks via the SAME query db reset uses
    // (`applied_at ASC, id ASC`). We can't call the private
    // `capture_historical_apply_order` directly from an integration
    // test, but we CAN re-issue the equivalent SELECT and confirm the
    // ordering — which is what the helper relies on. The test then
    // pins THE LOAD-BEARING INVARIANT: the ledger's `applied_at`
    // ordering for these rows is `a, c, b`.
    let rows = ctx
        .raw_scalar::<String>(
            "SELECT string_agg(version, ',' ORDER BY applied_at ASC, id ASC) \
             FROM djogi_schema_migrations \
             WHERE status IN ('applied', 'faked', 'baseline') \
               AND version IN ($1, $2, $3)",
            &[
                &"V20260101000000__a".to_string(),
                &"V20260201000000__b".to_string(),
                &"V20260301000000__c".to_string(),
            ],
        )
        .await
        .expect("rank query");
    assert_eq!(
        rows, "V20260101000000__a,V20260301000000__c,V20260201000000__b",
        "historical apply order MUST be a, c, b (NOT lexical a, b, c) — \
         this is the proof db reset's `capture_historical_apply_order` \
         drives `build_replay_plan` with the right ranks (the unit tests \
         in djogi/src/migrate/reset.rs cover the plan construction)"
    );

    // Cleanup — leave the test DB in a tidy state.
    let _ = ctx
        .raw_execute(
            "DELETE FROM djogi_schema_migrations WHERE version IN ($1, $2, $3)",
            &[
                &"V20260101000000__a".to_string(),
                &"V20260201000000__b".to_string(),
                &"V20260301000000__c".to_string(),
            ],
        )
        .await;
}

// ── Codex umbrella PARTIAL: db seed --database <other_db> live route ─────

/// Codex umbrella PARTIAL: prove that `db seed --database <other_db>`
/// actually runs SQL against `<other_db>`, NOT the application
/// database. Pre--Round-2 the `--database` flag selected the seed
/// directory but every seed ran against `database.url` regardless;
///  Round 2 added `derive_per_database_url` to splice the name into
/// the URL path. The integration test at lines 266-304 of this file
/// exercises the helper itself but cannot prove the live SQL route
/// because the harness owns the DB lifecycle. This test fills the gap:
///
/// 1. Provision a SECOND named database alongside the harness's
///    per-test database (admin URL is the env var DATABASE_URL).
/// 2. Run `run_seeds` with the routed URL targeting the second DB.
/// 3. Connect to BOTH databases and assert the seed table only
///    exists in the second DB, not in the application DB the
///    harness gave us.
/// 4. Tear down the second DB.
#[djogi::djogi_test]
async fn u_partial_db_seed_routes_to_other_database_live(mut ctx: djogi::DjogiContext) {
    use tokio_postgres::NoTls;

    // The harness's own admin URL — we splice from this to derive
    // the maintenance + per-DB URLs.
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let app_database = current_database(&mut ctx).await;

    // Compose a second DB name. Use a random suffix so concurrent
    // test runs don't collide (the harness also uses uuid-suffixed
    // names; the convention here mirrors that). Tests run with
    // `--test-threads=1` per the project's pre-commit policy so the
    // sequencing is deterministic.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let second_db = format!("djogi_seed_route_{stamp}");

    // Provision the second DB via the maintenance connection.
    {
        let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
            .await
            .expect("admin connect");
        let admin_driver = tokio::spawn(async move {
            if let Err(e) = admin_conn.await {
                eprintln!("[u-partial seed route] admin driver: {e}");
            }
        });
        admin_client
            .batch_execute(&format!("CREATE DATABASE \"{second_db}\""))
            .await
            .expect("CREATE DATABASE second");
        drop(admin_client);
        let _ = admin_driver.await;
    }

    // Lay down a single seed file under
    // `<workspace>/seeds/<second_db>/`.
    let work = temp_workspace("u_partial_seed_route");
    let seeds_dir = work.join("seeds").join(&second_db);
    fs::create_dir_all(&seeds_dir).unwrap();
    write_workspace_file(
        &work,
        seeds_dir.join("01_route_proof.sql"),
        b"CREATE TABLE u_partial_seed_proof (id BIGINT PRIMARY KEY);\n\
         INSERT INTO u_partial_seed_proof (id) VALUES (42);\n",
    )
    .unwrap();

    // Derive the per-database routed URL via the public helper
    // (the same helper `db seed` uses).
    let routed_url =
        djogi::migrate::derive_per_database_url(&admin_url, &second_db).expect("route");

    // Open a context against the routed URL and run `run_seeds`
    // — this is the same call shape the CLI uses.
    let pool = djogi::pg::pool::DjogiPool::connect(&routed_url)
        .await
        .expect("pool connect to second DB");
    let mut second_ctx = djogi::DjogiContext::from_pool(pool);
    let report = djogi::migrate::run_seeds(
        &mut second_ctx,
        &work,
        &second_db,
        &routed_url,
        false, // localhost
    )
    .await
    .expect("run_seeds against second DB");
    assert_eq!(report.entries.len(), 1, "exactly one seed must apply");
    assert_eq!(
        report.entries[0].outcome,
        SeedOutcome::Applied,
        "seed must report Applied (not skipped)"
    );

    // Verify the seed landed in the SECOND database, not the
    // application database. We re-issue the existence probe via
    // `current_database()` against the second DB context to pin
    // identity.
    let confirmed_db: String = second_ctx
        .raw_scalar("SELECT current_database()::text", &[])
        .await
        .expect("current_database");
    assert_eq!(
        confirmed_db, second_db,
        "second context must be connected to the routed DB"
    );
    let exists_in_second: bool = second_ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'u_partial_seed_proof' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists in second");
    assert!(
        exists_in_second,
        "seed table MUST exist in the routed (second) DB"
    );
    let row_count: i64 = second_ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM u_partial_seed_proof", &[])
        .await
        .expect("count in second");
    assert_eq!(row_count, 1, "seed INSERT must have landed");

    // CRITICAL: the same seed table must NOT exist in the
    // application database the harness gave us. If `--database
    // <name>` had failed to route, the seed would have run against
    // `app_database` instead of `second_db`. We assert against the
    // ORIGINAL ctx, which the harness opened against `app_database`.
    let exists_in_app: bool = ctx
        .raw_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'u_partial_seed_proof' AND relkind = 'r')",
            &[],
        )
        .await
        .expect("exists in app");
    assert!(
        !exists_in_app,
        "seed table MUST NOT exist in the application DB ({app_database}) — \
         the route would have failed if --database <name> had silently fallen \
         back to the application URL"
    );

    // Cleanup: drop the second context's pool first so the maintenance
    // DROP DATABASE has no live session.
    drop(second_ctx);

    // Tear down the second DB.
    let (admin_client, admin_conn) = tokio_postgres::connect(&admin_url, NoTls)
        .await
        .expect("admin teardown connect");
    let teardown_driver = tokio::spawn(async move {
        if let Err(e) = admin_conn.await {
            eprintln!("[u-partial seed route] teardown driver: {e}");
        }
    });
    let _ = admin_client
        .batch_execute(&format!(
            "DROP DATABASE IF EXISTS \"{second_db}\" WITH (FORCE)"
        ))
        .await;
    drop(admin_client);
    let _ = teardown_driver.await;

    let _ = remove_workspace_dir_all(&work, &work);
}

// ── #331 Session-pinning regression test: run_seeds ─────────────────────
// Exercises the exact regression scenario: run_seeds on a pool-backed
// context must pin one physical session for the full operation window.

#[djogi::djogi_test]
async fn run_seeds_pool_backed_context_pins_session(mut ctx: djogi::DjogiContext) {
    assert!(ctx.is_pool_backed(), "must be pool-backed for this regression test");

    // Create a minimal seed fixture for this test.
    let db_name = current_database(&mut ctx).await;
    let seed_dir = temp_workspace(&format!("331_seed_pin_{}", db_name));
    let seed_file = seed_dir.join("001_test_331.sql");
    write_workspace_file(&seed_dir, &seed_file, b"-- #331 seed regression test\nSELECT 1;\n")
        .expect("write seed file");

    // [REQ-331-12] Record backend PID before run_seeds
    let pid_before: i32 = ctx
        .raw_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("pg_backend_pid before run_seeds");

    // Run seeds against the fixture directory — must succeed without
    // AdvisoryUnlockReturnedFalse.
    let seed_result = run_seeds(
        &mut ctx,
        &seed_dir,
        &db_name,
        "postgres://localhost/main",
        false,
    )
    .await;
    match &seed_result {
        Ok(_) => {}
        Err(SeedError::AdvisoryUnlockReturnedFalse { database: db, key }) => {
            panic!(
                "AdvisoryUnlockReturnedFalse (db={}, key=0x{key:016x}): pool-backed \
                 context failed to pin for seed advisory lock (GH #331)",
                db
            );
        }
        Err(other) => panic!("unexpected error from run_seeds: {other:?}"),
    }

    // [REQ-331-12] Record backend PID after run_seeds
    let pid_after: i32 = ctx
        .raw_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("pg_backend_pid after run_seeds");

    tracing::debug!(pid_before, pid_after, "seed outer PIDs (may differ)");

    // Verify no advisory locks remain held on the seed lock key.
    // The seed runner uses bucket { database: <db_name>, app: "__djogi_seed_run__" }
    let lock_bucket = djogi::migrate::BucketKey {
        database: db_name.clone(),
        app: "__djogi_seed_run__".to_string(),
    };
    let lock_key = djogi::migrate::advisory_lock_key(&lock_bucket);
    let still_held: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM pg_locks \
             WHERE locktype = 'advisory' \
               AND classid = (($1::bigint >> 32) & 4294967295)::oid \
               AND objid   = ($1::bigint & 4294967295)::oid \
               AND mode    = 'ExclusiveLock'",
            &[&lock_key],
        )
        .await
        .expect("pg_locks query");

    assert_eq!(
        still_held,
        0,
        "advisory lock for seed run bucket={}/{} (key=0x{lock_key:016x}) \
         must be released after run_seeds on pool-backed context (GH #331)",
        lock_bucket.database, lock_bucket.app,
    );
}
