// `djogi analyze` end-to-end integration tests.
//
// # What these tests cover
//
// Three scenarios driven against the compiled `djogi` binary, each
// seeded into a fresh per-test database via `#[djogi_test]`:
//
// 1. **Healthy small table.** ~50 live rows, no deletes, no
//    partitions, threshold-vacuum and threshold-partition-rows at
//    sensible defaults → `Recommendation::Healthy`.
// 2. **High dead-tuple ratio.** 100 rows inserted, 50 deleted, then
//    `ANALYZE` to populate `pg_stat_user_tables.n_dead_tup` →
//    `Recommendation::VacuumNeeded` (50 / 100 = 0.5 > 0.2).
// 3. **Large unpartitioned table.** 200 rows inserted, no
//    partitions, `--threshold-partition-rows 100` so the rule fires
//    against a tractable test fixture (the spec's 10M default would
//    require a multi-minute seed) → `Recommendation::PartitionRecommended`.
//
// # `--threshold-partition-rows` override (200-row fixture)
//
// The plan calls for a "> 10M-row unpartitioned" table to
// trigger `PartitionRecommended`. Seeding 10M rows on every CI run
// would dominate the test budget. The partition-rows
// threshold was lifted onto the CLI flag specifically so the test can drop it
// to 100 — the rule is "live-row count strictly above the
// threshold", and 200 > 100 exercises the same code path with a
// 0.04% of the rows. The semantic equivalence is mechanical: there
// is no row-count-dependent branch inside [`recommend`], only the
// threshold comparison.
//
// # Single-DB simplification
//
// Mirrors the verify-CLI test (`djogi_verify_cli`).
// The `#[djogi_test]` harness provisions ONE per-test database; we
// point `DATABASE_URL` (and the audit-DB env override) at the same
// database so the spawned `djogi analyze` binary connects to the
// same Postgres state we just seeded. The fixture tables share the
// `analyze_fixture_` prefix so we can filter the output by that substring and
// ignore any framework tables (e.g. `_djogi_seed_runs`) the harness
// creates as part of provisioning.
//
// # `pg_stat_user_tables` populated via explicit `ANALYZE`
//
// Postgres updates `n_live_tup` / `n_dead_tup` either when
// autovacuum sweeps the table (asynchronous, latency-bounded by the
// collector) or when an explicit `ANALYZE table_name` runs (which
// also samples and updates the planner statistics). Test code calls
// `ANALYZE` after seeding so the binary subprocess reads the
// intended counts deterministically — without it, every counter is
// zero and every recommendation collapses to `Healthy`.
//
// # Shared CLI helpers
//
// `djogi_binary_path`, `current_database`, `temp_workspace`, and
// `write_minimal_djogi_toml` live in `djogi::testing::cli` (gated behind
// the `testing` feature) so this file and its sibling
// `djogi_verify_cli.rs` share a single implementation. See
// djogi#119.
//
// # Spec / memory anchors
//
// - v3 plan — single integration test against a real DB.
// - Plan specs for analyze CLI recommendations.
// - `djogi-cli/src/analyze.rs` — the implementation under test.

use std::fs;
use std::path::Path;
use std::process::Command;

use djogi::testing::cli::{
    current_database, djogi_binary_path, temp_workspace, write_minimal_djogi_toml,
};

/// Common prefix for every fixture table this file creates. Used to
/// filter the analyze output so framework-managed tables provisioned
/// by `#[djogi_test]` (e.g. `_djogi_seed_runs`) cannot cause
/// false-positive matches when we walk the JSON array.
const FIXTURE_PREFIX: &str = "analyze_fixture_";

fn test_database_url(database: &str) -> String {
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    splice_db_into_url(&admin_url, database)
}

fn splice_db_into_url(url: &str, new_db: &str) -> String {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("postgres://") {
        ("postgres://", rest)
    } else if let Some(rest) = url.strip_prefix("postgresql://") {
        ("postgresql://", rest)
    } else {
        panic!("DATABASE_URL must be a postgres:// or postgresql:// URL, got {url}");
    };

    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    let query = tail.find('?').map_or("", |idx| &tail[idx..]);

    format!("{scheme}{authority}/{new_db}{query}")
}

/// Poll `pg_stat_user_tables.n_dead_tup` for `table` until it
/// reports `> 0`, retrying with a short sleep between checks.
///
/// Postgres 18's cumulative stats system writes per-relation
/// counters (`n_dead_tup`, `n_tup_del`, etc.) to shared memory on
/// the backend that ran the DDL/DML, but those writes become
/// visible to other backends only after the writing backend's
/// transaction commits and the stats subsystem flushes the
/// in-memory snapshot. The `raw_execute` calls above each commit
/// independently (auto-commit on every pool checkout), but a fresh
/// connection that lands microseconds later can still race the
/// visibility window — we have observed `n_dead_tup = 0` in the
/// `djogi analyze` subprocess immediately after `ANALYZE` returned
/// to the harness session.
///
/// Polling (rather than a fixed sleep) keeps the test fast in the
/// common case (one extra round-trip when stats are already
/// visible) while guaranteeing correctness on slower hosts. The
/// 5-second cap is generous — empirically the stats settle in
/// under 10ms — but bounded so a permanent failure surfaces as a
/// clear panic rather than an infinite hang.
async fn wait_for_dead_tuples(ctx: &mut djogi::DjogiContext, table: &str) {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    // `raw_rows` (not `raw_scalar`) so a "no row yet" outcome —
    // possible when the stats subsystem hasn't seen the relation
    // at all yet — surfaces as an empty Vec rather than a
    // `NotFound` error we'd have to special-case.
    let sql = "SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = $1";
    loop {
        // Each `raw_rows` call checks out a fresh connection from
        // the pool, so the read sees the latest committed stats
        // rather than any session-local cache.
        let rows = ctx
            .raw_rows(sql, &[&table])
            .await
            .expect("read pg_stat_user_tables.n_dead_tup");
        let dead: i64 = rows.first().map(|row| row.get::<_, i64>(0)).unwrap_or(0);
        if dead > 0 {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "n_dead_tup stayed 0 for `{table}` after 5s; the cumulative stats system \
                 should have flushed by now"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll `pg_stat_user_tables.n_live_tup` for `table` until it
/// reports `>= min_count`, retrying with a short sleep between
/// checks.
///
/// Sibling of [`wait_for_dead_tuples`] that targets the live-tuple
/// counter instead. The same cumulative-stats visibility window
/// applies: `ANALYZE` updates `n_live_tup` from a fresh sample, but
/// the new value becomes visible to other backends only after the
/// writing backend's transaction commits and the stats subsystem
/// flushes its in-memory snapshot. A subprocess that connects
/// microseconds after `ANALYZE` returned to the harness can race
/// the visibility window — observed empirically as
/// `n_live_tup = 0` (and therefore the partition-rule threshold
/// gate failing to fire) in the analyze CLI for the large-table
/// fixture, even though seeding completed and `ANALYZE` ran.
///
/// The threshold-comparison rule in [`recommend`] is strict
/// greater-than (`n_live_tup > threshold_partition_rows`), so
/// callers should pass `threshold + 1` for `min_count` to
/// guarantee the rule fires once the wait returns.
async fn wait_for_live_tuples(ctx: &mut djogi::DjogiContext, table: &str, min_count: i64) {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(5);
    let sql = "SELECT n_live_tup FROM pg_stat_user_tables WHERE relname = $1";
    loop {
        let rows = ctx
            .raw_rows(sql, &[&table])
            .await
            .expect("read pg_stat_user_tables.n_live_tup");
        let live: i64 = rows.first().map(|row| row.get::<_, i64>(0)).unwrap_or(0);
        if live >= min_count {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "n_live_tup stayed below {min_count} for `{table}` after 5s \
                 (last observed: {live}); the cumulative stats system should \
                 have flushed by now"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Spawn `djogi analyze --format json` against `workspace` with the
/// supplied threshold flags and return the parsed JSON array.
///
/// Threads the per-test DB URL through both `DATABASE_URL` (read by
/// adopters who set `database.url = "${DATABASE_URL}"`) and as the
/// raw `database.url` in the temp `Djogi.toml`, mirroring the
/// verify-CLI test. The audit-DB env override points at the same DB
/// per the single-DB simplification documented in the module header.
fn run_analyze_json(
    workspace: &Path,
    db_url: &str,
    threshold_partition_rows: i64,
) -> Vec<serde_json::Value> {
    let bin = djogi_binary_path();
    assert!(
        bin.is_file(),
        "djogi binary not found at {} — run `cargo build -p djogi-cli` first",
        bin.display(),
    );
    let output = Command::new(&bin)
        .arg("analyze")
        .arg("--workspace")
        .arg(workspace)
        .arg("--format")
        .arg("json")
        .arg("--threshold-partition-rows")
        .arg(threshold_partition_rows.to_string())
        .env("DATABASE_URL", db_url)
        .env("DJOGI_CRUD_LOG_URL", db_url)
        .output()
        .expect("spawn djogi analyze");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "djogi analyze failed (exit {:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code(),
    );

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("parse analyze JSON: {e}\nstdout: {stdout}"));
    parsed
        .as_array()
        .unwrap_or_else(|| panic!("analyze output must be a JSON array; got: {parsed}"))
        .clone()
}

/// Find the analyze row whose `table_name` ends with the supplied
/// suffix. The full table name is `schemaname.relname` (e.g.
/// `public.analyze_fixture_healthy_<n>`); seeding always lands in `public`,
/// so suffix-matching keeps the assertion robust against schema
/// changes without coupling to the literal `public.` prefix.
fn find_row_by_suffix<'a>(rows: &'a [serde_json::Value], suffix: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|row| {
            row.get("table_name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.ends_with(suffix))
        })
        .unwrap_or_else(|| {
            let names: Vec<String> = rows
                .iter()
                .filter_map(|r| {
                    r.get("table_name")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .collect();
            panic!("no row with table_name ending in `{suffix}`; saw: {names:?}")
        })
}

/// Extract the recommendation `kind` discriminant from one analyze row.
fn recommendation_kind(row: &serde_json::Value) -> &str {
    row.get("recommendation")
        .and_then(|r| r.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or_else(|| panic!("row missing recommendation.kind: {row}"))
}

#[djogi::djogi_test]
async fn analyze_healthy_small_table_returns_healthy(mut ctx: djogi::DjogiContext) {
    // Seed: a small table with 50 live rows, no deletes, no
    // partitions. Default thresholds (0.2 vacuum, override the
    // partition-rows ceiling far above the seeded count) → no rule
    // fires → Healthy.
    let table = format!("{FIXTURE_PREFIX}healthy");
    ctx.raw_execute(
        &format!("CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY, name TEXT)"),
        &[],
    )
    .await
    .expect("create healthy table");
    ctx.raw_execute(
        &format!("INSERT INTO {table} (name) SELECT 'x' FROM generate_series(1, 50)"),
        &[],
    )
    .await
    .expect("seed healthy table");
    ctx.raw_execute(&format!("ANALYZE {table}"), &[])
        .await
        .expect("analyze healthy table");

    let database = current_database(&mut ctx).await;
    let test_url = test_database_url(&database);
    let workspace = temp_workspace("analyze-healthy");
    write_minimal_djogi_toml(&workspace, &test_url);

    // 1_000_000 keeps the partition rule far away from a 50-row
    // table — well above the seeded count, well below the i64 max
    // so we exercise the same code path operators see in production.
    let rows = run_analyze_json(&workspace, &test_url, 1_000_000);

    // Filter to fixture-prefixed rows so framework tables can't
    // contaminate the assertion.
    let fixture_rows: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|row| {
            row.get("table_name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.contains(FIXTURE_PREFIX))
        })
        .collect();
    assert_eq!(
        fixture_rows.len(),
        1,
        "expected exactly one analyze_fixture_-prefixed row; saw: {fixture_rows:?}",
    );
    let row = find_row_by_suffix(&rows, &table);
    assert_eq!(
        recommendation_kind(row),
        "healthy",
        "expected Healthy for a 50-row table with no deletes; row: {row}",
    );

    if let Ok(temp_canon) = std::env::temp_dir().canonicalize()
        && workspace.starts_with(&temp_canon)
    {
        let _ = fs::remove_dir_all(&workspace);
    }
}

#[djogi::djogi_test]
async fn analyze_high_dead_tuple_ratio_returns_vacuum_needed(mut ctx: djogi::DjogiContext) {
    // Seed: 100 rows, then DELETE 50, then ANALYZE so
    // `pg_stat_user_tables.n_dead_tup` reflects the deleted rows.
    // Resulting ratio: 50 / 100 = 0.5 → strictly greater than the
    // 0.2 default vacuum threshold → VacuumNeeded.
    let table = format!("{FIXTURE_PREFIX}vacuum");
    ctx.raw_execute(
        &format!("CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY, name TEXT)"),
        &[],
    )
    .await
    .expect("create vacuum table");
    ctx.raw_execute(
        &format!("INSERT INTO {table} (name) SELECT 'x' FROM generate_series(1, 100)"),
        &[],
    )
    .await
    .expect("seed vacuum table");
    ctx.raw_execute(&format!("DELETE FROM {table} WHERE id <= 50"), &[])
        .await
        .expect("delete half");
    // ANALYZE primes the planner stats and updates `n_live_tup` to
    // reflect the post-delete sample. The cumulative-stats counters
    // (`n_dead_tup`, `n_tup_del`) are written to shared memory by
    // the backend that ran the DELETE — but those writes are
    // visible to OTHER backends only after the writing backend
    // commits and the stats are flushed. PG18 makes that flush
    // immediate-on-commit, but a fresh subprocess that connects
    // microseconds later can still race the visibility window.
    //
    // Below we poll a fresh pool connection until `n_dead_tup > 0`
    // is observable, which proves the analyze subprocess will see
    // the same value when it connects.
    ctx.raw_execute(&format!("ANALYZE {table}"), &[])
        .await
        .expect("analyze vacuum table");
    wait_for_dead_tuples(&mut ctx, &table).await;

    let database = current_database(&mut ctx).await;
    let test_url = test_database_url(&database);
    let workspace = temp_workspace("analyze-vacuum");
    write_minimal_djogi_toml(&workspace, &test_url);

    // Keep partition rule out of the way (1M >> 50 live rows).
    let rows = run_analyze_json(&workspace, &test_url, 1_000_000);
    let row = find_row_by_suffix(&rows, &table);
    assert_eq!(
        recommendation_kind(row),
        "vacuum_needed",
        "expected VacuumNeeded for a 50-live / 50-dead table; row: {row}",
    );

    // Sanity: the carried `dead_tup_ratio` must be a finite number
    // strictly above the 0.2 threshold. We don't pin the exact value
    // because Postgres may report slightly different live/dead
    // counts depending on autovacuum / HOT cleanup timing, but the
    // ratio MUST be over the threshold or `recommend` would not
    // have returned this arm.
    let ratio = row
        .get("recommendation")
        .and_then(|r| r.get("dead_tup_ratio"))
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| panic!("missing dead_tup_ratio: {row}"));
    assert!(
        ratio.is_finite() && ratio > 0.2,
        "dead_tup_ratio must be finite and >0.2; got {ratio}",
    );

    if let Ok(temp_canon) = std::env::temp_dir().canonicalize()
        && workspace.starts_with(&temp_canon)
    {
        let _ = fs::remove_dir_all(&workspace);
    }
}

#[djogi::djogi_test]
async fn analyze_large_unpartitioned_returns_partition_recommended(mut ctx: djogi::DjogiContext) {
    // Seed: 200 live rows in an unpartitioned table; pass
    // `--threshold-partition-rows 100` so 200 > 100 fires the
    // PartitionRecommended rule. The 10M-row default threshold from
    // the spec would dominate the test budget; the threshold flag
    // exists precisely so this test can exercise the rule on a
    // tractable fixture.
    let table = format!("{FIXTURE_PREFIX}partition");
    ctx.raw_execute(
        &format!("CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY, name TEXT)"),
        &[],
    )
    .await
    .expect("create partition table");
    ctx.raw_execute(
        &format!("INSERT INTO {table} (name) SELECT 'x' FROM generate_series(1, 200)"),
        &[],
    )
    .await
    .expect("seed partition table");
    ctx.raw_execute(&format!("ANALYZE {table}"), &[])
        .await
        .expect("analyze partition table");
    // ANALYZE writes `n_live_tup` to the cumulative stats system,
    // but a fresh subprocess that connects microseconds later can
    // race the visibility window — observed as `n_live_tup = 0` in
    // the analyze CLI even after `ANALYZE` returned. The
    // partition-rule gate is `n_live_tup > 100` (strict
    // greater-than), so we wait until at least 101 is observable
    // through a fresh pool connection before spawning the binary.
    wait_for_live_tuples(&mut ctx, &table, 101).await;

    let database = current_database(&mut ctx).await;
    let test_url = test_database_url(&database);
    let workspace = temp_workspace("analyze-partition");
    write_minimal_djogi_toml(&workspace, &test_url);

    // Threshold of 100 with 200 seeded rows → strictly greater
    // than → PartitionRecommended.
    let rows = run_analyze_json(&workspace, &test_url, 100);
    let row = find_row_by_suffix(&rows, &table);
    assert_eq!(
        recommendation_kind(row),
        "partition_recommended",
        "expected PartitionRecommended for 200 live rows / 100 threshold; row: {row}",
    );

    // The reason field must reference the row count and the
    // threshold — the human renderer and operator scripts grep for
    // these substrings, so the JSON contract pins them too.
    let reason = row
        .get("recommendation")
        .and_then(|r| r.get("reason"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing recommendation.reason: {row}"));
    assert!(
        reason.contains("200") && reason.contains("not partitioned"),
        "reason must cite row count + 'not partitioned'; got: {reason}",
    );
    assert!(
        reason.contains("threshold: 100"),
        "reason must echo the override threshold; got: {reason}",
    );

    if let Ok(temp_canon) = std::env::temp_dir().canonicalize()
        && workspace.starts_with(&temp_canon)
    {
        let _ = fs::remove_dir_all(&workspace);
    }
}
