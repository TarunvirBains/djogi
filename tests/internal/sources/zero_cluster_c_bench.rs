// Cluster C C4 (T20b) — release-mode smoke benchmarks for the
// Cluster C surface (`convex_hull` aggregate + `.qualify(...)` derived-table
// lowering).
//
// These are *not* perf guarantees. They are smoke bounds + grep-able runtime
// numbers that close the `[UNVERIFIED]` perf-claim gaps flagged in the v3
// plan amendment (2026-04-30). Two scenarios:
//
// 1. **`bench_convex_hull_aggregate_1000_points`** — 1000 GeoPoints
//    distributed across 100 herds (10 points/herd); fold each group via
//    `FieldRef::convex_hull()`. Confirms the spatial aggregate composes at
//    non-trivial fan-in and decodes 100 hulls in well under the soft cap.
//
// 2. **`bench_qualify_top_n_10000_rows_100_partitions`** — 10000 rows
//    partitioned 100 ways via `herd_id`; runs `RowNumber.qualify(rn ≤ 3)`
//    (framework lowering) AND a hand-written derived-table equivalent in
//    raw SQL. Both runtimes are recorded; the assertion is that the
//    framework path stays within a 3× envelope of the hand-written shape
//    — i.e. the lowering doesn't introduce a structural regression on top
//    of what the planner would already produce. (The wrapping
//    `SELECT * FROM (...) AS __djogi_q WHERE ...` should fold identically
//    to the inline form; 3× absorbs row-decode overhead and CI jitter.)
//
// ## Running
//
// ```bash
// cargo test --test phase8_zero_cluster_c_bench -p djogi --all-features \
//     --release -- --test-threads=1 --nocapture
// ```
//
// Debug-mode runs also pass — the soft caps below are loose enough — but
// the printed runtime is host-sensitive and not a perf claim.
//
// ## Why `tests/`, not `benches/`
//
// Same rationale as `phase8_zero_pool_bench` and `phase8_zero_tree_query_bench`:
// cargo's `[[bench]]` harness pulls in nightly criterion-style infra we don't
// want for v0.1.0 smoke checks. Stuffing the timing logic into ordinary
// `#[djogi_test]` bodies keeps the test surface single-tracked and reuses
// the per-test-database harness.
//
// ## Feature gate
//
// The whole file is gated on `feature = "spatial"` because the convex-hull
// bench exercises the spatial surface. The window bench technically does
// not need `spatial`, but co-locating both Cluster C benches in one file
// keeps the cluster's bench fixture single-file and matches the cluster's
// identity (Cluster C ships both surfaces together).

use std::time::{Duration, Instant};

use djogi::geo::{GeoPoint, Polygon};
use djogi::prelude::*;

// ── Models ──────────────────────────────────────────────────────────────────

#[model(table = "phase8c_bench_points", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct BenchPoint {
    pub herd_id: i64,
    pub location: GeoPoint,
}

#[model(table = "phase8c_bench_window", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct BenchWindowRow {
    pub herd_id: i64,
    pub score: i64,
    pub label: String,
}

// ── Banner / scale knobs ────────────────────────────────────────────────────

fn banner(name: &str) {
    let mode = if cfg!(debug_assertions) {
        "debug (numbers are NOT perf claims; smoke bounds only)"
    } else {
        "release (smoke bounds; not perf guarantees)"
    };
    println!();
    println!("=== {name} ===");
    println!("mode: {mode}");
}

/// Iteration-count knob — debug builds get a smaller fixture so the suite
/// stays snappy when run inadvertently in `cargo test` defaults.
const fn scale(release: usize, debug: usize) -> usize {
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

// ── 1. convex_hull aggregate over 1000 points / 100 herds ──────────────────

#[djogi::djogi_test(extensions = ["postgis"])]
async fn bench_convex_hull_aggregate_1000_points(mut ctx: djogi::DjogiContext) {
    banner("bench_convex_hull_aggregate_1000_points");
    let n_herds: i64 = scale(100, 25) as i64;
    let per_herd: i64 = 10;
    let n_points = (n_herds * per_herd) as usize;

    ctx.raw_ddl(
        "CREATE TABLE phase8c_bench_points (
             id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
             created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
             updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
             herd_id    BIGINT      NOT NULL,
             location   GEOGRAPHY(Point, 4326) NOT NULL
         );
         CREATE INDEX phase8c_bench_points_loc_gix
             ON phase8c_bench_points USING GIST(location);",
    )
    .await
    .expect("DDL must succeed");

    // Seed in a single round-trip via generate_series. Each herd `h` gets
    // `per_herd` points distributed in a real 2D grid around its center —
    // `k` is split into x/y offsets via `k % 5` and `k / 5` so the per-herd
    // points span both axes (5×2 grid for per_herd=10). A 1D arrangement
    // would collapse to a LineString under ST_ConvexHull and the typed
    // FieldRef::convex_hull() would fail EWKB decode — convex_hull is
    // typed as AggregateExpr<Polygon> and needs a non-degenerate hull.
    //
    // lat is shifted into a safe band well inside [-90, 90]; lon stays
    // inside [-180, 180] for any reasonable n_herds.
    ctx.raw_execute(
        "INSERT INTO phase8c_bench_points (herd_id, location)
         SELECT
             (h - 1)::bigint AS herd_id,
             ST_SetSRID(
                 ST_MakePoint(
                     (h * 0.5) + ((k % 5) * 0.05),
                     (h * 0.3) + ((k / 5) * 0.05) - 30.0
                 ),
                 4326
             )::geography
         FROM generate_series(1, $1::int) AS h
         CROSS JOIN generate_series(0, $2::int - 1) AS k",
        &[&(n_herds as i32), &(per_herd as i32)],
    )
    .await
    .expect("seed points");

    // ANALYZE so the planner has stats before the timed query.
    ctx.raw_ddl("ANALYZE phase8c_bench_points")
        .await
        .expect("analyze must succeed");

    let total: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM phase8c_bench_points", &[])
        .await
        .expect("count seeded");
    println!("seeded {total} points across {n_herds} herds");
    assert_eq!(
        total as usize, n_points,
        "seed count must match (n_herds × per_herd)"
    );

    let start = Instant::now();
    let hulls: Vec<(i64, Polygon)> = BenchPoint::objects()
        .group_by(|f| f.herd_id())
        .annotate(|f| f.location().convex_hull())
        .fetch_all(&mut ctx)
        .await
        .expect("convex_hull aggregate must execute");
    let elapsed = start.elapsed();

    println!(
        "[bench] convex_hull aggregate {total} points / {n_herds} herds: \
         {elapsed:?} ({} hulls returned)",
        hulls.len(),
    );

    assert_eq!(hulls.len(), n_herds as usize, "expected one hull per herd");
    assert!(
        elapsed < Duration::from_secs(30),
        "convex_hull bench exceeded 30s soft cap: {elapsed:?}"
    );
}

// ── 2. RowNumber.qualify lowering vs hand-written derived table ────────────

#[djogi::djogi_test]
async fn bench_qualify_top_n_10000_rows_100_partitions(mut ctx: djogi::DjogiContext) {
    banner("bench_qualify_top_n_10000_rows_100_partitions");
    let n_herds: i64 = scale(100, 25) as i64;
    let per_herd: i64 = scale(100, 40) as i64;
    let n_rows = (n_herds * per_herd) as usize;

    ctx.raw_ddl(
        "CREATE TABLE phase8c_bench_window (
             id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
             created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
             updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
             herd_id    BIGINT      NOT NULL,
             score      BIGINT      NOT NULL,
             label      TEXT        NOT NULL
         );",
    )
    .await
    .expect("window DDL must succeed");

    // Seed 10000 rows in a single round trip. herd_id distributed by
    // generate_series modulo, score derived from a deterministic hash so
    // the bench is reproducible across runs (random() would re-roll on
    // each invocation and add noise to ratio comparisons).
    ctx.raw_execute(
        "INSERT INTO phase8c_bench_window (herd_id, score, label)
         SELECT
             (g % $1::int)::bigint                                   AS herd_id,
             ((g * 2654435761)::bigint % 1000000)::bigint            AS score,
             'r' || g::text                                          AS label
         FROM generate_series(1, $2::int) AS g",
        &[&(n_herds as i32), &(n_rows as i32)],
    )
    .await
    .expect("seed window rows");

    ctx.raw_ddl("ANALYZE phase8c_bench_window")
        .await
        .expect("analyze must succeed");

    let total: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM phase8c_bench_window", &[])
        .await
        .expect("count seeded");
    println!("seeded {total} rows across {n_herds} herds");
    assert_eq!(
        total as usize, n_rows,
        "seed count must match (n_herds × per_herd)"
    );

    // (a) Framework path — typed RowNumber + .qualify(rn ≤ 3) lowered to a
    //     derived-table outer-WHERE because PG18 has no QUALIFY clause.
    let start_fw = Instant::now();
    let framework_rows: Vec<(BenchWindowRow, i64)> = BenchWindowRow::objects()
        .annotate(|f| {
            RowNumber::new()
                .partition_by(f.herd_id())
                .order_by(f.score().desc())
                .alias("rank")
        })
        .qualify(|w| w.lte(3))
        .fetch_all(&mut ctx)
        .await
        .expect("framework qualify query must execute");
    let framework_elapsed = start_fw.elapsed();

    // Hand-written path selects the SAME column set AND decodes each
    // column into the SAME Rust type as the framework path. Per
    // `BenchWindowRow`'s macro emission, the canonical column set is
    // (id: HeerId, created_at: DateTime, updated_at: DateTime,
    // herd_id: i64, score: i64, label: String) — id is a `djogi::HeerId`,
    // not a bare `i64`, because `pk = HeerId` injects the typed wrapper.
    // The raw tuple decodes id as HeerId so the wire-level codec
    // invocation is symmetric — bare `i64` decode would route through a
    // different postgres-types FromSql impl and skew the comparison.
    //
    // The framework path can NOT skip created_at / updated_at because
    // they are mandatory model fields; selecting only the user-visible
    // subset on the raw side would let the planner prune them and skip
    // both wire transfer and decode, masking projection regressions in
    // the derived-table lowering.
    let start_hw = Instant::now();
    let hand_rows: Vec<(
        djogi::HeerId,
        djogi::DateTime,
        djogi::DateTime,
        i64,
        i64,
        String,
        i64,
    )> = ctx
        .raw_rows(
            "SELECT id, created_at, updated_at, herd_id, score, label, rn
             FROM (
                 SELECT id, created_at, updated_at, herd_id, score, label,
                        ROW_NUMBER() OVER (PARTITION BY herd_id ORDER BY score DESC) AS rn
                 FROM phase8c_bench_window
             ) AS sub
             WHERE rn <= 3",
            &[],
        )
        .await
        .expect("hand-written equivalent must run")
        .into_iter()
        .map(|row| {
            (
                row.get::<_, djogi::HeerId>("id"),
                row.get::<_, djogi::DateTime>("created_at"),
                row.get::<_, djogi::DateTime>("updated_at"),
                row.get::<_, i64>("herd_id"),
                row.get::<_, i64>("score"),
                row.get::<_, String>("label"),
                row.get::<_, i64>("rn"),
            )
        })
        .collect();
    let hand_elapsed = start_hw.elapsed();

    println!(
        "[bench] qualify framework path:    {framework_elapsed:?} \
         ({} rows decoded)",
        framework_rows.len(),
    );
    println!(
        "[bench] qualify hand-written path: {hand_elapsed:?} \
         ({} rows decoded)",
        hand_rows.len(),
    );

    // Sanity — both paths must agree on the row count.
    assert_eq!(
        framework_rows.len(),
        hand_rows.len(),
        "framework and hand-written paths must agree on row count"
    );
    assert_eq!(
        framework_rows.len() as i64,
        n_herds * 3,
        "expected exactly 3 rows per herd (top-3 qualify)"
    );

    // Soft caps + regression guard. 30s leaves plenty of headroom; the
    // ratio assertion is the load-bearing claim — the framework lowering
    // must not introduce a structural regression vs the hand-written
    // shape. Skip the ratio assertion when the hand-written path runs in
    // under 5ms (timing jitter dominates and the ratio isn't informative).
    assert!(
        framework_elapsed < Duration::from_secs(30),
        "framework qualify bench exceeded 30s soft cap: {framework_elapsed:?}"
    );
    assert!(
        hand_elapsed < Duration::from_secs(30),
        "hand-written equivalent exceeded 30s soft cap: {hand_elapsed:?}"
    );
    if hand_elapsed > Duration::from_millis(5) {
        let ratio = framework_elapsed.as_secs_f64() / hand_elapsed.as_secs_f64();
        println!("[bench] framework / hand-written ratio: {ratio:.2}x");
        assert!(
            ratio < 3.0,
            "framework qualify lowering regressed: {ratio:.2}x hand-written \
             ({framework_elapsed:?} vs {hand_elapsed:?})"
        );
    } else {
        println!("[bench] hand-written path under 5ms — ratio comparison skipped");
    }
}
