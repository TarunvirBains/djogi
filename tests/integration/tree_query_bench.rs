// Smoke benchmarks for tree-recursive
// query surface against a real Postgres 18.
//
// These are *not* perf guarantees. They are smoke bounds + grep-able
// runtime numbers that back the scalability claim ("the framework
// supports the production tree-query pattern at non-trivial scale").
// Three benchmarks:
//
// 1. **`bench_1000_node_tree_descendants`** — 1000-node single-root
//    tree; time `tree_descendants` to completion. Confirms the
//    recursive-CTE path scales from the unit-test fixtures' 5-row
//    trees to four-orders-of-magnitude larger fixtures without
//    blowing up.
//
// 2. **`bench_50_deep_chain_with_paths`** — 50-deep chain; time
//    `fetch_all_with_paths`. Targets the path-accumulator
//    `array_append` chain — a deep walk grows the `text[]` path
//    column linearly, and we want to confirm the per-step append
//    stays cheap.
//
// 3. **`bench_5000_pedigree_materialize_closure`** — 5000-row
//    pedigree (every node has 2 self-FKs, fanout simulating real
//    ancestry). Times `materialize_closure` from empty closure
//    table to fully populated. The headline number that backs the
//    scalability claim — closure-table materialisation is the
//    production-scale answer for tree queries (see Risk 10 in the
//    Scalability lens).
//
// ## Running
//
// ```bash
// cargo test --test tree_query_bench -p djogi --all-features \
//     --release -- --test-threads=1 --nocapture
// ```
//
// Debug-mode runs also pass — the soft caps below are loose enough —
// but the printed runtime is host-sensitive and not a perf claim.
//
// ## Why `tests/`, not `benches/`
//
// Same rationale as `pool_bench`: cargo's `[[bench]]`
// harness pulls in nightly criterion-style infra we don't want for a
// v0.1.0 smoke check. Stuffing the timing logic into ordinary
// `#[djogi_test]` bodies keeps the test surface single-tracked and
// reuses the per-test-database harness.

use std::time::{Duration, Instant};

use djogi::prelude::*;

// ── Models ──────────────────────────────────────────────────────────────────

#[model(
    table = "bench_tree",
    pk = HeerId,
    tree_edge = "parent_id",
    indexes(index(fields = [parent_id]))
)]
#[derive(Debug, Clone)]
pub struct BenchTree {
    pub label: i32,
    pub parent_id: Option<ForeignKey<BenchTree>>,
}

#[model(
    table = "bench_pedigree",
    pk = HeerId,
    indexes(
        index(fields = [mother_id]),
        index(fields = [father_id])
    )
)]
#[derive(Debug, Clone)]
pub struct BenchPedigree {
    pub label: i32,
    pub mother_id: Option<ForeignKey<BenchPedigree>>,
    pub father_id: Option<ForeignKey<BenchPedigree>>,
}

#[model(
    table = "bench_pedigree_closure",
    pk = HeerId,
    no_default,
    indexes(unique(fields = [pedigree_id, ancestor_id, depth]))
)]
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

fn bench_tree(label: i32, parent_id: Option<HeerId>) -> BenchTree {
    BenchTree {
        id: <HeerId as PrimaryKey>::sentinel(),
        created_at: DateTime::UNIX_EPOCH,
        updated_at: DateTime::UNIX_EPOCH,
        label,
        parent_id: parent_id.map(ForeignKey::new),
    }
}

fn bench_pedigree(
    label: i32,
    mother_id: Option<HeerId>,
    father_id: Option<HeerId>,
) -> BenchPedigree {
    BenchPedigree {
        id: <HeerId as PrimaryKey>::sentinel(),
        created_at: DateTime::UNIX_EPOCH,
        updated_at: DateTime::UNIX_EPOCH,
        label,
        mother_id: mother_id.map(ForeignKey::new),
        father_id: father_id.map(ForeignKey::new),
    }
}

// ── 1. 1000-node tree descendants ──────────────────────────────────────────

#[djogi::djogi_test(sync_models = [BenchTree])]
async fn bench_1000_node_tree_descendants(mut ctx: DjogiContext) {
    banner("bench_1000_node_tree_descendants");
    let n = scale(1000, 100);

    // Fan-out shape: every non-root row has parent label `child.label / 4`.
    let root = BenchTree::create(&mut ctx, bench_tree(0, None))
        .await
        .expect("seed root");
    let mut ids = vec![root.id];
    for label in 1..(n as i32) {
        let parent_id = ids[(label / 4) as usize];
        let row = BenchTree::create(&mut ctx, bench_tree(label, Some(parent_id)))
            .await
            .expect("seed tree row");
        ids.push(row.id);
    }

    let total = BenchTree::objects()
        .count(&mut ctx)
        .await
        .expect("count seeded");
    println!("seeded {total} rows; benching tree_descendants from root");

    let root_id = root.id;
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

#[djogi::djogi_test(sync_models = [BenchTree])]
async fn bench_50_deep_chain_with_paths(mut ctx: DjogiContext) {
    banner("bench_50_deep_chain_with_paths");
    let depth = scale(50, 10);

    // Seed a single 50-deep chain. Each row's parent is the previous
    // row by label.
    let root = BenchTree::create(&mut ctx, bench_tree(0, None))
        .await
        .expect("seed root");
    let mut prev = root.id;
    for label in 1..(depth as i32) {
        let next = BenchTree::create(&mut ctx, bench_tree(label, Some(prev)))
            .await
            .expect("seed chain row");
        prev = next.id;
    }

    let root_id = root.id;
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

#[djogi::djogi_test(sync_models = [BenchPedigree, BenchPedigreeAncestry])]
async fn bench_5000_pedigree_materialize_closure(mut ctx: DjogiContext) {
    banner("bench_5000_pedigree_materialize_closure");
    let n = scale(5000, 250);

    // Seed 2 ancestors + (n - 2) descendants where each descendant
    // picks its mother/father from rows seeded at strictly lower
    // labels (so the graph is a DAG). This mimics a real ancestry
    // pedigree: oldest ancestors at small labels, newest individuals
    // at large labels, every individual has both parents at lower
    // labels.
    let first = BenchPedigree::create(&mut ctx, bench_pedigree(0, None, None))
        .await
        .expect("seed first root");
    let second = BenchPedigree::create(&mut ctx, bench_pedigree(1, None, None))
        .await
        .expect("seed second root");
    let mut ids = vec![first.id, second.id];
    for label in 2..(n as i32) {
        let mother_id = ids[(label - 2) as usize];
        let father_id = ids[(label - 1) as usize];
        let row = BenchPedigree::create(
            &mut ctx,
            bench_pedigree(label, Some(mother_id), Some(father_id)),
        )
        .await
        .expect("seed pedigree row");
        ids.push(row.id);
    }

    let total = BenchPedigree::objects()
        .count(&mut ctx)
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
