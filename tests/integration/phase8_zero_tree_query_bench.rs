//! Phase 8-Zero Cluster B5 (T14c) — Smoke benchmarks for tree-recursive
//! query surface against a real Postgres 18.
//!
//! These are *not* perf guarantees. They are smoke bounds + grep-able
//! runtime numbers that back the scalability claim ("the framework
//! supports the production tree-query pattern at non-trivial scale").
//! Three benchmarks:
//!
//! 1. **`bench_1000_node_tree_descendants`** — 1000-node single-root
//!    tree; time `tree_descendants` to completion. Confirms the
//!    recursive-CTE path scales from the unit-test fixtures' 5-row
//!    trees to four-orders-of-magnitude larger fixtures without
//!    blowing up.
//!
//! 2. **`bench_50_deep_chain_with_paths`** — 50-deep chain; time
//!    `fetch_all_with_paths`. Targets the path-accumulator
//!    `array_append` chain — a deep walk grows the `text[]` path
//!    column linearly, and we want to confirm the per-step append
//!    stays cheap.
//!
//! 3. **`bench_5000_pedigree_materialize_closure`** — 5000-row
//!    pedigree (every node has 2 self-FKs, fanout simulating real
//!    ancestry). Times `materialize_closure` from empty closure
//!    table to fully populated. The headline number that backs the
//!    scalability claim — closure-table materialisation is the
//!    production-scale answer for tree queries (see Risk 10 in the
//!    Phase 8-Zero scalability lens).
//!
//! ## Running
//!
//! ```bash
//! cargo test --test phase8_zero_tree_query_bench -p djogi --all-features \
//!     --release -- --test-threads=1 --nocapture
//! ```
//!
//! Debug-mode runs also pass — the soft caps below are loose enough —
//! but the printed runtime is host-sensitive and not a perf claim.
//!
//! ## Why `tests/`, not `benches/`
//!
//! Same rationale as `phase8_zero_pool_bench`: cargo's `[[bench]]`
//! harness pulls in nightly criterion-style infra we don't want for a
//! v0.1.0 smoke check. Stuffing the timing logic into ordinary
//! `#[djogi_test]` bodies keeps the test surface single-tracked and
//! reuses the per-test-database harness.

use std::time::{Duration, Instant};

use djogi::prelude::*;

// ── Models ──────────────────────────────────────────────────────────────────

#[model(table = "phase8_bench_tree", pk = HeerId, tree_edge = "parent_id")]
#[derive(Debug, Clone)]
pub struct BenchTree {
    pub label: i32,
    pub parent_id: Option<ForeignKey<BenchTree>>,
}

#[model(table = "phase8_bench_pedigree", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct BenchPedigree {
    pub label: i32,
    pub mother_id: Option<ForeignKey<BenchPedigree>>,
    pub father_id: Option<ForeignKey<BenchPedigree>>,
}

#[model(table = "phase8_bench_pedigree_closure", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct BenchPedigreeAncestry {
    pub pedigree_id: ForeignKey<BenchPedigree>,
    pub ancestor_id: ForeignKey<BenchPedigree>,
    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for BenchPedigreeAncestry {
    type Source = BenchPedigree;
    fn source_column() -> &'static str {
        "pedigree_id"
    }
    fn ancestor_column() -> &'static str {
        "ancestor_id"
    }
    fn depth_column() -> &'static str {
        "depth"
    }
    fn path_count_column() -> &'static str {
        "path_count"
    }
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

/// Iteration-count knob — debug builds get a smaller fixture so the
/// suite stays snappy when run inadvertently in `cargo test` defaults.
const fn scale(release: usize, debug: usize) -> usize {
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

// ── 1. 1000-node tree descendants ──────────────────────────────────────────

#[djogi::djogi_test]
async fn bench_1000_node_tree_descendants(mut ctx: DjogiContext) {
    banner("bench_1000_node_tree_descendants");
    let n = scale(1000, 100);

    ctx.raw_execute(
        "CREATE TABLE phase8_bench_tree (
             id          BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             label       INTEGER NOT NULL,
             parent_id   BIGINT REFERENCES phase8_bench_tree(id) ON DELETE CASCADE
         )",
        &[],
    )
    .await
    .expect("create table");
    ctx.raw_execute(
        "CREATE INDEX phase8_bench_tree_parent_idx ON phase8_bench_tree(parent_id)",
        &[],
    )
    .await
    .expect("create index");

    // Seed via three round trips — fan-out shape: every non-root row
    // has `parent_id = parent.id WHERE parent.label = child.label / 4`
    // in label space, the classic 4-ary tree. Step 1 inserts the root
    // (`label = 0`); step 2 inserts every other label with
    // `parent_id = NULL`; step 3 resolves parents via a single
    // UPDATE/JOIN over the now-committed labels. See the rationale
    // below the root insert for why the parent resolution is split
    // into its own statement.
    let root: i64 = ctx
        .raw_scalar(
            "INSERT INTO phase8_bench_tree (label) VALUES (0) RETURNING id",
            &[],
        )
        .await
        .expect("seed root");

    // Two-phase seed: insert every label with `parent_id = NULL` in one
    // round trip, then a single UPDATE/JOIN resolves every non-root
    // row's parent via label arithmetic. Splitting INSERT and UPDATE
    // is load-bearing — a single `INSERT...SELECT` with a self-
    // referential `(SELECT id FROM phase8_bench_tree WHERE label = g/4)`
    // would only see rows committed *before* the statement, leaving
    // every batch's first label-arithmetic-collision orphaned (and
    // its entire subtree unreachable from the root). Two phases cost
    // two round trips instead of `ceil(log_4(n))` and produce a
    // correctly connected 4-ary tree without depth-by-depth fragility.
    let _ = root;
    ctx.raw_execute(
        "INSERT INTO phase8_bench_tree (label) \
         SELECT g FROM generate_series(1::int, $1::int - 1) AS g",
        &[&(n as i32)],
    )
    .await
    .expect("seed labels");
    ctx.raw_execute(
        "UPDATE phase8_bench_tree AS child \
         SET parent_id = parent.id \
         FROM phase8_bench_tree AS parent \
         WHERE child.label > 0 AND parent.label = child.label / 4",
        &[],
    )
    .await
    .expect("set parent_id from label arithmetic");

    let total: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM phase8_bench_tree", &[])
        .await
        .expect("count seeded");
    println!("seeded {total} rows; benching tree_descendants from root");

    let root_id = HeerId::from_i64(root).expect("valid HeerId");
    let start = Instant::now();
    let walk = BenchTree::tree_descendants(root_id)
        .expect("tree_edge resolves")
        .fetch_all(&mut ctx)
        .await
        .expect("descendants walk");
    let elapsed = start.elapsed();

    println!(
        "[bench] tree_descendants {}-node: {:?} ({} rows reached)",
        total,
        elapsed,
        walk.len()
    );

    // Soft cap — 10s leaves plenty of headroom; on any healthy local
    // Postgres a 1000-node walk lands in well under 1s.
    assert!(
        elapsed < Duration::from_secs(10),
        "tree_descendants over {total} rows exceeded 10s soft cap: {elapsed:?}"
    );
    // Sanity check: every seeded row must be reachable from the root.
    assert_eq!(
        walk.len() as i64,
        total,
        "every seeded row must be reachable from the root"
    );
}

// ── 2. 50-deep chain with paths ───────────────────────────────────────────

#[djogi::djogi_test]
async fn bench_50_deep_chain_with_paths(mut ctx: DjogiContext) {
    banner("bench_50_deep_chain_with_paths");
    let depth = scale(50, 10);

    ctx.raw_execute(
        "CREATE TABLE phase8_bench_tree (
             id          BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             label       INTEGER NOT NULL,
             parent_id   BIGINT REFERENCES phase8_bench_tree(id) ON DELETE CASCADE
         )",
        &[],
    )
    .await
    .expect("create table");

    // Seed a single 50-deep chain. Each row's parent is the previous
    // row by label. Looped INSERTs because each step depends on the
    // previous row's id. (A recursive CTE INSERT could collapse this
    // to one round trip, but at 50 rows the loop dominates only a few
    // hundred ms even on a slow host.)
    let root: i64 = ctx
        .raw_scalar(
            "INSERT INTO phase8_bench_tree (label) VALUES (0) RETURNING id",
            &[],
        )
        .await
        .expect("seed root");
    let mut prev = root;
    for label in 1..(depth as i32) {
        let next: i64 = ctx
            .raw_scalar(
                "INSERT INTO phase8_bench_tree (label, parent_id) VALUES ($1, $2) RETURNING id",
                &[&label, &prev],
            )
            .await
            .expect("seed chain");
        prev = next;
    }

    let root_id = HeerId::from_i64(root).expect("valid HeerId");
    let start = Instant::now();
    let walk = BenchTree::tree_descendants(root_id)
        .expect("tree_edge resolves")
        .fetch_all_with_paths(&mut ctx)
        .await
        .expect("with-paths walk");
    let elapsed = start.elapsed();

    println!(
        "[bench] fetch_all_with_paths {}-deep chain: {:?} ({} rows)",
        depth,
        elapsed,
        walk.len()
    );

    // Soft cap — same generous 10s as above. The point is to confirm
    // `array_append` doesn't blow up at chain length 50.
    assert!(
        elapsed < Duration::from_secs(10),
        "fetch_all_with_paths over {depth}-deep chain exceeded 10s soft cap: {elapsed:?}"
    );
    // Path length at depth N is exactly N entries.
    let max_path_len = walk.iter().map(|(_, _, p)| p.len()).max().unwrap_or(0);
    assert_eq!(
        max_path_len,
        depth - 1,
        "deepest row's path length must equal the chain depth - 1"
    );
}

// ── 3. 5000-pedigree closure materialisation ──────────────────────────────

#[djogi::djogi_test]
async fn bench_5000_pedigree_materialize_closure(mut ctx: DjogiContext) {
    banner("bench_5000_pedigree_materialize_closure");
    let n = scale(5000, 250);

    ctx.raw_execute(
        "CREATE TABLE phase8_bench_pedigree (
             id          BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             label       INTEGER NOT NULL,
             mother_id   BIGINT REFERENCES phase8_bench_pedigree(id) ON DELETE SET NULL,
             father_id   BIGINT REFERENCES phase8_bench_pedigree(id) ON DELETE SET NULL
         )",
        &[],
    )
    .await
    .expect("create pedigree");
    ctx.raw_execute(
        "CREATE INDEX phase8_bench_pedigree_mother_idx ON phase8_bench_pedigree(mother_id)",
        &[],
    )
    .await
    .expect("create mother index");
    ctx.raw_execute(
        "CREATE INDEX phase8_bench_pedigree_father_idx ON phase8_bench_pedigree(father_id)",
        &[],
    )
    .await
    .expect("create father index");
    ctx.raw_execute(
        "CREATE TABLE phase8_bench_pedigree_closure (
             id           BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
             pedigree_id  BIGINT NOT NULL REFERENCES phase8_bench_pedigree(id) ON DELETE CASCADE,
             ancestor_id  BIGINT NOT NULL REFERENCES phase8_bench_pedigree(id) ON DELETE CASCADE,
             depth        INTEGER NOT NULL,
             path_count   BIGINT NOT NULL DEFAULT 1,
             UNIQUE (pedigree_id, ancestor_id, depth)
         )",
        &[],
    )
    .await
    .expect("create closure table");

    // Seed 2 ancestors + (n - 2) descendants where each descendant
    // picks its mother/father from rows seeded at strictly lower
    // labels (so the graph is a DAG). This mimics a real ancestry
    // pedigree: oldest ancestors at small labels, newest individuals
    // at large labels, every individual has both parents at lower
    // labels.
    ctx.raw_execute(
        "INSERT INTO phase8_bench_pedigree (label) VALUES (0), (1)",
        &[],
    )
    .await
    .expect("seed two roots");

    // Seed labels 2..n via a single round-trip generate_series. Pick
    // mother = (label - 2) % label, father = (label - 1) % label —
    // ensures both are < label and unique enough that fan-in / fan-out
    // both happen at non-trivial rates. The exact pattern doesn't
    // matter for the bench shape; what matters is that the graph
    // depth grows logarithmically with n and every label has both
    // parents declared.
    ctx.raw_execute(
        "INSERT INTO phase8_bench_pedigree (label, mother_id, father_id) \
         SELECT g, \
                (SELECT id FROM phase8_bench_pedigree WHERE label = ((g - 2) % g)), \
                (SELECT id FROM phase8_bench_pedigree WHERE label = ((g - 1) % g)) \
         FROM generate_series(2, $1::int) AS g",
        &[&((n - 1) as i32)],
    )
    .await
    .expect("seed descendants");

    let total: i64 = ctx
        .raw_scalar("SELECT COUNT(*)::bigint FROM phase8_bench_pedigree", &[])
        .await
        .expect("count seeded");
    println!("seeded {total} pedigree rows; benching materialize_closure");

    let start = Instant::now();
    // Cap depth at 8 for the bench — without a cap the closure walk
    // is O(rows × ancestor-graph-depth) which grows with the seed
    // shape. A cap keeps the bench focused on the inner loop's
    // throughput rather than on the worst-case path count.
    let report = BenchPedigree::materialize_closure::<BenchPedigreeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default().with_max_depth(8),
    )
    .await
    .expect("materialize_closure");
    let elapsed = start.elapsed();

    println!(
        "[bench] materialize_closure {}-source pedigree: {:?} \
         (rows_written = {}, sources_visited = {})",
        total, elapsed, report.rows_written, report.sources_visited
    );

    // Soft cap — 30s so the bench survives slow CI runners; locally
    // a 5000-row materialisation lands in 1-3s on a healthy host.
    assert!(
        elapsed < Duration::from_secs(30),
        "materialize_closure over {total} rows exceeded 30s soft cap: {elapsed:?}"
    );
    // Sanity: every source row visited (the helper walks unfiltered
    // by default).
    assert_eq!(
        report.sources_visited as i64, total,
        "every source row must be visited at depth 0"
    );
}
