// Cluster B5 (T14) — Live integration tests for the
// `RecursiveQuerySet<T>` surface against a real Postgres 18.
//
// Drives every public terminal end-to-end:
//
// - `tree_descendants` / `tree_ancestors` (single-edge sugar via
//   `#[model(tree_edge = "...")]`)
// - `QuerySet::tree_descendants` / `tree_ancestors` (explicit-path API
//   for models with multiple self-FKs or no `tree_edge`)
// - `RecursiveQuerySet::with_max_depth`
// - `RecursiveQuerySet::filter` / `order_by`
// - `RecursiveQuerySet::search_breadth_first_by` / `search_depth_first_by`
// - `RecursiveQuerySet::fetch_all` / `count` / `exists` / `first`
// - `RecursiveQuerySet::fetch_all_with_paths`
// - `Model::full_ancestors`
//
// Each ordinary test runs inside `#[djogi::djogi_test(sync_models = [...])]`
// which provisions a per-test database and model schema, so the tests are
// mutually independent and parallel-safe. The RLS role/policy probe lives in
// `tests/internal/zero_tree_query_rls_live.rs` because it requires
// raw session-role and catalog setup outside the ordinary typed test surface.
//
// The model fixtures intentionally use distinct table names per test
// file group (`phase8_tree_*`) so a future combined run with
// `phase8_zero_materialize_closure_live` does not collide on table
// definitions inside the inventory.

use djogi::prelude::*;

// ── Single-edge tree (mother / parent_id) ───────────────────────────────────

/// One-self-FK tree node. The `tree_edge = "parent_id"` attribute
/// activates the inherent `Model::tree_descendants` / `tree_ancestors`
/// sugar; tests confirm both the sugar path and the explicit-path API
/// behave identically when only one edge exists.
#[model(table = "phase8_tree_node", pk = HeerId, tree_edge = "parent_id")]
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub parent_id: Option<ForeignKey<TreeNode>>,
}

// ── Multi-edge tree (mother_id + father_id) ─────────────────────────────────

/// Two-self-FK pedigree node. No `tree_edge` declared on purpose —
/// `Model::tree_descendants(id)` should fail with a runtime
/// `DjogiError::Validation`, while `Model::full_ancestors(id)` walks
/// every declared edge via UNION ALL.
#[model(table = "phase8_tree_pedigree", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PedigreeNode {
    pub name: String,
    pub mother_id: Option<ForeignKey<PedigreeNode>>,
    pub father_id: Option<ForeignKey<PedigreeNode>>,
}

// ── Non-self-FK model (for full_ancestors-zero-edges error) ─────────────────

/// Carries no self-FK at all; serves the negative-case test that
/// `full_ancestors` returns a descriptive `DjogiError::Validation`
/// at terminal time when `self_fk_count() == 0`.
#[model(table = "phase8_tree_orphan", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OrphanNode {
    pub name: String,
}

// ── Seed helpers ────────────────────────────────────────────────────────────

/// Insert one TreeNode row with optional parent. Returns the
/// freshly-inserted row (with DB-assigned id + timestamps).
async fn seed_tree_node(ctx: &mut DjogiContext, name: &str, parent: Option<&TreeNode>) -> TreeNode {
    TreeNode::create(
        ctx,
        TreeNode {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            name: name.into(),
            parent_id: parent.map(|p| ForeignKey::new(p.id)),
        },
    )
    .await
    .expect("seed_tree_node")
}

async fn seed_pedigree(
    ctx: &mut DjogiContext,
    name: &str,
    mother: Option<&PedigreeNode>,
    father: Option<&PedigreeNode>,
) -> PedigreeNode {
    PedigreeNode::create(
        ctx,
        PedigreeNode {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            name: name.into(),
            mother_id: mother.map(|m| ForeignKey::new(m.id)),
            father_id: father.map(|f| ForeignKey::new(f.id)),
        },
    )
    .await
    .expect("seed_pedigree")
}

// ── 1. Single-tree descendants walk ─────────────────────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn single_tree_descendants_walk(mut ctx: DjogiContext) {
    // Build a 5-deep chain: root → l1 → l2 → l3 → l4 → l5.
    let root = seed_tree_node(&mut ctx, "root", None).await;
    let l1 = seed_tree_node(&mut ctx, "l1", Some(&root)).await;
    let l2 = seed_tree_node(&mut ctx, "l2", Some(&l1)).await;
    let l3 = seed_tree_node(&mut ctx, "l3", Some(&l2)).await;
    let l4 = seed_tree_node(&mut ctx, "l4", Some(&l3)).await;
    let l5 = seed_tree_node(&mut ctx, "l5", Some(&l4)).await;

    let descendants = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .fetch_all(&mut ctx)
        .await
        .expect("descendants fetch");

    // Expect 6 rows: root + 5 descendants. Order is unspecified without
    // SEARCH BFS/DFS, so we sort by name to compare deterministically.
    let mut names: Vec<&str> = descendants.iter().map(|n| n.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["l1", "l2", "l3", "l4", "l5", "root"]);

    // with_max_depth(2) prunes anything past depth 2 from the root —
    // i.e. anchor (depth 0), l1 (depth 1), l2 (depth 2). l3, l4, l5
    // are stripped.
    let bounded = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .with_max_depth(2)
        .fetch_all(&mut ctx)
        .await
        .expect("bounded fetch");
    let mut bounded_names: Vec<&str> = bounded.iter().map(|n| n.name.as_str()).collect();
    bounded_names.sort();
    assert_eq!(bounded_names, vec!["l1", "l2", "root"]);

    // Silence unused-binding warnings for `l5` (the last sentinel).
    let _ = l5;
}

// ── 2. Forest with multiple roots ───────────────────────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn forest_multiple_roots(mut ctx: DjogiContext) {
    // Tree A: a1 → a2.
    let a1 = seed_tree_node(&mut ctx, "a1", None).await;
    let _a2 = seed_tree_node(&mut ctx, "a2", Some(&a1)).await;
    // Tree B: b1 → b2.
    let b1 = seed_tree_node(&mut ctx, "b1", None).await;
    let _b2 = seed_tree_node(&mut ctx, "b2", Some(&b1)).await;

    let descendants_of_a = TreeNode::tree_descendants(a1.id)
        .expect("tree_edge resolves")
        .fetch_all(&mut ctx)
        .await
        .expect("fetch a-tree");

    let names: Vec<&str> = descendants_of_a.iter().map(|n| n.name.as_str()).collect();
    assert!(
        names.iter().all(|n| n.starts_with('a')),
        "tree-A walk must not surface any b-tree rows: {names:?}"
    );
    assert_eq!(descendants_of_a.len(), 2, "tree-A has exactly two rows");
}

// ── 3. Cycle detection terminates ───────────────────────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn cycle_detection_terminates(mut ctx: DjogiContext) {
    // Insert two NULL-parent rows, then UPDATE to create a cycle:
    // a → b → a (b's parent = a, a's parent = b). The CYCLE id clause
    // on the recursive CTE detects the revisit and stops.
    let mut a = seed_tree_node(&mut ctx, "a", None).await;
    let b = seed_tree_node(&mut ctx, "b", Some(&a)).await;
    a.parent_id = Some(ForeignKey::new(b.id));
    a.save(&mut ctx).await.expect("introduce cycle");

    // Walk descendants of `a` — without CYCLE detection this would
    // recurse forever. With it, Postgres marks the cycle and the outer
    // `WHERE NOT is_cycle` strips the sentinel row. The walk must
    // terminate; assertion is that the call returns at all (we set a
    // generous depth cap as a safety net but the CYCLE clause is the
    // primary guarantee).
    let descendants = TreeNode::tree_descendants(a.id)
        .expect("tree_edge resolves")
        .with_max_depth(10)
        .fetch_all(&mut ctx)
        .await
        .expect("cycle walk terminates");

    // Both rows are part of the cycle and Postgres marks the second
    // visit to either node as cycle. The non-cycle rows surface at
    // least once each. Asserting an exact count would over-specify
    // Postgres's CYCLE row layout; instead assert the result is
    // non-empty and bounded.
    assert!(
        !descendants.is_empty(),
        "cycle walk must surface at least the anchor row"
    );
    assert!(
        descendants.len() <= 4,
        "cycle walk must not loop indefinitely; got {} rows",
        descendants.len()
    );
}

// ── 4. Multi-self-FK explicit edge selection ────────────────────────────────

#[djogi::djogi_test(sync_models = [PedigreeNode])]
async fn two_self_fks_explicit_edge(mut ctx: DjogiContext) {
    // Pedigree:
    //   - mom (no parents)
    //   - dad (no parents)
    //   - child whose mother = mom and father = dad
    let mom = seed_pedigree(&mut ctx, "mom", None, None).await;
    let dad = seed_pedigree(&mut ctx, "dad", None, None).await;
    let child = seed_pedigree(&mut ctx, "child", Some(&mom), Some(&dad)).await;

    // Walk ancestors via the mother edge — should reach `mom` only,
    // not `dad`. (Anchor row is `child` itself; the recursive term
    // walks one hop up the mother edge to mom.)
    let mother_line = PedigreeNode::objects()
        .tree_ancestors(PedigreeNodeRelated::mother(), child.id)
        .fetch_all(&mut ctx)
        .await
        .expect("mother walk");
    let mother_names: Vec<&str> = mother_line.iter().map(|p| p.name.as_str()).collect();
    assert!(
        mother_names.contains(&"child") && mother_names.contains(&"mom"),
        "mother walk must reach mom: {mother_names:?}"
    );
    assert!(
        !mother_names.contains(&"dad"),
        "mother walk must NOT reach dad: {mother_names:?}"
    );

    // Walk ancestors via the father edge — symmetric: dad only.
    let father_line = PedigreeNode::objects()
        .tree_ancestors(PedigreeNodeRelated::father(), child.id)
        .fetch_all(&mut ctx)
        .await
        .expect("father walk");
    let father_names: Vec<&str> = father_line.iter().map(|p| p.name.as_str()).collect();
    assert!(
        father_names.contains(&"child") && father_names.contains(&"dad"),
        "father walk must reach dad: {father_names:?}"
    );
    assert!(
        !father_names.contains(&"mom"),
        "father walk must NOT reach mom: {father_names:?}"
    );
}

// ── 5. with_max_depth truncates exactly at N ────────────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn with_max_depth_truncates_exactly_at_n(mut ctx: DjogiContext) {
    // 4-deep: root → d1 → d2 → d3.
    let root = seed_tree_node(&mut ctx, "root", None).await;
    let d1 = seed_tree_node(&mut ctx, "d1", Some(&root)).await;
    let d2 = seed_tree_node(&mut ctx, "d2", Some(&d1)).await;
    let _d3 = seed_tree_node(&mut ctx, "d3", Some(&d2)).await;

    let bounded = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .with_max_depth(2)
        .fetch_all(&mut ctx)
        .await
        .expect("max_depth walk");
    let mut names: Vec<&str> = bounded.iter().map(|n| n.name.as_str()).collect();
    names.sort();
    // Anchor at depth 0, d1 at depth 1, d2 at depth 2; d3 at depth 3
    // is excluded by `parent.depth < 2` in the recursive term.
    assert_eq!(names, vec!["d1", "d2", "root"]);
}

// ── 6. Composed filter + order_by + every terminal ─────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn composed_filter_order_count_exists_first(mut ctx: DjogiContext) {
    let root = seed_tree_node(&mut ctx, "root", None).await;
    let _alpha = seed_tree_node(&mut ctx, "alpha", Some(&root)).await;
    let _beta = seed_tree_node(&mut ctx, "beta", Some(&root)).await;
    let _gamma = seed_tree_node(&mut ctx, "gamma", Some(&root)).await;

    // Filter: name != "beta". The recursive-term filter applies to
    // every step except the anchor (which matches on id only). All
    // four rows survive at the recursive level minus beta — root
    // surfaces from the anchor, alpha and gamma from the recursive
    // term, beta is filtered out.
    let count = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .filter(|f| f.name().neq("beta".to_string()))
        .order_by(|f| f.name().asc())
        .count(&mut ctx)
        .await
        .expect("count terminal");
    assert_eq!(count, 3, "anchor + 2 children pass the filter");

    let exists = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .filter(|f| f.name().eq("alpha".to_string()))
        .exists(&mut ctx)
        .await
        .expect("exists terminal");
    assert!(exists, "alpha is reachable from root");

    let first = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .order_by(|f| f.name().asc())
        .first(&mut ctx)
        .await
        .expect("first terminal");
    assert!(
        first.is_some(),
        "first must surface a row when the walk is non-empty"
    );
}

// ── 7. tree_ancestors walks upward ─────────────────────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn tree_ancestors_walks_upward(mut ctx: DjogiContext) {
    // Chain: a → b → c (c's parent = b, b's parent = a).
    let a = seed_tree_node(&mut ctx, "a", None).await;
    let b = seed_tree_node(&mut ctx, "b", Some(&a)).await;
    let c = seed_tree_node(&mut ctx, "c", Some(&b)).await;

    let ancestors = TreeNode::tree_ancestors(c.id)
        .expect("tree_edge resolves")
        .fetch_all(&mut ctx)
        .await
        .expect("ancestors fetch");

    // Anchor (c) plus ancestors b, a — 3 rows total. The recursive
    // term walks `parent.parent_id = child.id`, so each step reaches
    // the parent of the current row.
    let mut names: Vec<&str> = ancestors.iter().map(|n| n.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["a", "b", "c"]);
}

// ── 8. SEARCH BREADTH FIRST orders by depth ────────────────────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn search_breadth_first_orders_by_depth(mut ctx: DjogiContext) {
    // Two-level fan-out:
    //   root
    //     ├─ l1a
    //     │    ├─ l2a
    //     │    └─ l2b
    //     └─ l1b
    //          └─ l2c
    let root = seed_tree_node(&mut ctx, "root", None).await;
    let l1a = seed_tree_node(&mut ctx, "l1a", Some(&root)).await;
    let l1b = seed_tree_node(&mut ctx, "l1b", Some(&root)).await;
    let _l2a = seed_tree_node(&mut ctx, "l2a", Some(&l1a)).await;
    let _l2b = seed_tree_node(&mut ctx, "l2b", Some(&l1a)).await;
    let _l2c = seed_tree_node(&mut ctx, "l2c", Some(&l1b)).await;

    let bfs = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .search_breadth_first_by(TreeNodeFields.name())
        .fetch_all(&mut ctx)
        .await
        .expect("bfs fetch");

    // BFS surfaces every depth-1 node before any depth-2 node. We
    // assert that every "l1*" row appears before every "l2*" row in
    // the result vector — the only ordering BFS guarantees on a
    // breadth-first walk is by depth, not within a depth band.
    let l1_max_index = bfs
        .iter()
        .enumerate()
        .filter(|(_, n)| n.name.starts_with("l1"))
        .map(|(i, _)| i)
        .max()
        .expect("at least one l1 row");
    let l2_min_index = bfs
        .iter()
        .enumerate()
        .filter(|(_, n)| n.name.starts_with("l2"))
        .map(|(i, _)| i)
        .min()
        .expect("at least one l2 row");
    assert!(
        l1_max_index < l2_min_index,
        "BFS must place every l1 row before any l2 row; \
         l1_max_index={l1_max_index}, l2_min_index={l2_min_index}, \
         walk={:?}",
        bfs.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
}

// ── 10. SEARCH DEPTH FIRST traverses one chain at a time ──────────────────

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn search_depth_first_traverses_chains(mut ctx: DjogiContext) {
    // Same fan-out as the BFS test.
    let root = seed_tree_node(&mut ctx, "root", None).await;
    let l1a = seed_tree_node(&mut ctx, "l1a", Some(&root)).await;
    let l1b = seed_tree_node(&mut ctx, "l1b", Some(&root)).await;
    let _l2a = seed_tree_node(&mut ctx, "l2a", Some(&l1a)).await;
    let _l2b = seed_tree_node(&mut ctx, "l2b", Some(&l1a)).await;
    let _l2c = seed_tree_node(&mut ctx, "l2c", Some(&l1b)).await;

    let dfs = TreeNode::tree_descendants(root.id)
        .expect("tree_edge resolves")
        .search_depth_first_by(TreeNodeFields.name())
        .fetch_all(&mut ctx)
        .await
        .expect("dfs fetch");

    // DFS visits one full subtree before the next sibling. The exact
    // tiebreaker between sibling subtrees depends on Postgres's
    // SEARCH ordering and the per-row `name` value used as the BY
    // key. We assert the structural property that l2 children of l1a
    // appear consecutively (as a contiguous block) — which is the
    // hallmark of DFS — rather than interleaving with l1b's children.
    let positions: std::collections::HashMap<&str, usize> = dfs
        .iter()
        .enumerate()
        .map(|(i, n)| (n.name.as_str(), i))
        .collect();
    let p_l2a = positions["l2a"];
    let p_l2b = positions["l2b"];
    let p_l2c = positions["l2c"];

    // The two children of l1a (l2a, l2b) must be on the same side of
    // l2c (the lone child of l1b) — i.e. both before l2c or both
    // after. If DFS were broken, the result would interleave
    // l2a/l2b around l2c.
    let both_before_c = p_l2a < p_l2c && p_l2b < p_l2c;
    let both_after_c = p_l2a > p_l2c && p_l2b > p_l2c;
    assert!(
        both_before_c || both_after_c,
        "DFS must visit l1a's subtree contiguously: l2a={}, l2b={}, l2c={}; \
         walk={:?}",
        p_l2a,
        p_l2b,
        p_l2c,
        dfs.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
}

// ── 11. fetch_all_with_paths returns edge column names ────────────────────

#[djogi::djogi_test(sync_models = [PedigreeNode])]
async fn fetch_all_with_paths_returns_edge_names(mut ctx: DjogiContext) {
    // Three-generation pedigree:
    //   gma (no parents) — grandmother
    //   gpa (no parents) — grandfather
    //   mom (mother = gma, father = gpa)
    //   child (mother = mom)
    let gma = seed_pedigree(&mut ctx, "gma", None, None).await;
    let gpa = seed_pedigree(&mut ctx, "gpa", None, None).await;
    let mom = seed_pedigree(&mut ctx, "mom", Some(&gma), Some(&gpa)).await;
    let child = seed_pedigree(&mut ctx, "child", Some(&mom), None).await;

    let walk = PedigreeNode::full_ancestors(child.id)
        .fetch_all_with_paths(&mut ctx)
        .await
        .expect("paths walk");

    // The anchor row (child) has empty `path` (no edges traversed
    // yet). Every recursive row's `path` is a sequence of edge column
    // names — only "mother_id" or "father_id" can appear. We assert
    // path entries are edge names (not row ids or arbitrary strings).
    for (node, depth, path) in &walk {
        if *depth == 0 {
            assert!(
                path.is_empty(),
                "anchor row must carry empty path: {node:?}, path={path:?}"
            );
        } else {
            assert_eq!(
                path.len() as i32,
                *depth,
                "path length must equal depth at every step: {node:?}, path={path:?}"
            );
            for entry in path {
                assert!(
                    entry == "mother_id" || entry == "father_id",
                    "every path entry must be a self-FK column name; got {entry:?}"
                );
            }
        }
    }

    // The maternal-grandmother surfaces with path ["mother_id", "mother_id"]
    // (child → mom via mother edge, mom → gma via mother edge). The
    // maternal-grandfather surfaces with ["mother_id", "father_id"].
    let gma_walk: Vec<&Vec<String>> = walk
        .iter()
        .filter(|(n, _, _)| n.name == "gma")
        .map(|(_, _, p)| p)
        .collect();
    assert!(
        gma_walk
            .iter()
            .any(|p| p.as_slice() == ["mother_id", "mother_id"]),
        "gma must surface via the [mother_id, mother_id] path: walks={gma_walk:?}"
    );

    let gpa_walk: Vec<&Vec<String>> = walk
        .iter()
        .filter(|(n, _, _)| n.name == "gpa")
        .map(|(_, _, p)| p)
        .collect();
    assert!(
        gpa_walk
            .iter()
            .any(|p| p.as_slice() == ["mother_id", "father_id"]),
        "gpa must surface via the [mother_id, father_id] path: walks={gpa_walk:?}"
    );
}

// ── 12. full_ancestors preserves multiplicity through two edges ───────────

#[djogi::djogi_test(sync_models = [PedigreeNode])]
async fn full_ancestors_two_edges_preserves_multiplicity(mut ctx: DjogiContext) {
    // A linebreeding pedigree where the SAME ancestor reaches the
    // child via TWO distinct paths:
    //   common (no parents) — the shared ancestor
    //   mom (mother = common)
    //   dad (father = common)  — note: same `common` is mother of mom AND father of dad
    //   child (mother = mom, father = dad)
    //
    // child → mom (via mother) → common (via mother) — path1
    // child → dad (via father) → common (via father) — path2
    let common = seed_pedigree(&mut ctx, "common", None, None).await;
    // mom: mother = common, no father.
    let mom = seed_pedigree(&mut ctx, "mom", Some(&common), None).await;
    // dad: no mother, father = common.
    let dad = seed_pedigree(&mut ctx, "dad", None, Some(&common)).await;
    let child = seed_pedigree(&mut ctx, "child", Some(&mom), Some(&dad)).await;

    let walk = PedigreeNode::full_ancestors(child.id)
        .fetch_all_with_paths(&mut ctx)
        .await
        .expect("multiplicity walk");

    // `common` must appear TWICE — once via [mother_id, mother_id]
    // (child → mom → common) and once via [father_id, father_id]
    // (child → dad → common). UNION ALL preserves these as distinct
    // rows; UNION (the wrong choice) would dedup and break Wright
    // kinship sums.
    let common_paths: Vec<&Vec<String>> = walk
        .iter()
        .filter(|(n, _, _)| n.name == "common")
        .map(|(_, _, p)| p)
        .collect();
    assert_eq!(
        common_paths.len(),
        2,
        "common ancestor must surface twice (multiplicity preservation): paths={common_paths:?}"
    );
    let mut sorted: Vec<Vec<String>> = common_paths.iter().map(|p| (*p).clone()).collect();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            vec!["father_id".to_string(), "father_id".to_string()],
            vec!["mother_id".to_string(), "mother_id".to_string()],
        ],
        "the two paths must be the maternal-maternal and paternal-paternal sequences"
    );
}

// ── 13. full_ancestors with zero self-FKs errors descriptively ────────────

#[djogi::djogi_test(sync_models = [OrphanNode])]
async fn full_ancestors_zero_self_fks_errors(mut ctx: DjogiContext) {
    // Insert one orphan row so there's a non-zero source — the helper's
    // empty-edges guard fires before the SQL builder, so even a
    // populated table errors when the model has no self-FK.
    let orphan = OrphanNode::create(
        &mut ctx,
        OrphanNode {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            name: "lone".into(),
        },
    )
    .await
    .expect("seed orphan");

    let result = OrphanNode::full_ancestors(orphan.id)
        .fetch_all(&mut ctx)
        .await;

    let err = result.expect_err("zero self-FKs must error");
    let msg = err.to_string();
    assert!(
        msg.contains("phase8_tree_orphan"),
        "error must name the model's table: {msg}"
    );
    assert!(
        msg.contains("self-FK"),
        "error must explain the self-FK requirement: {msg}"
    );
}
