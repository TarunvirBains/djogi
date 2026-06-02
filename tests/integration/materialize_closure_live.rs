// Live integration tests for
// `Model::materialize_closure` against a real Postgres 18.
//
// Drives the full closure-helper surface end-to-end:
//
// - Self-pair triples at depth 0 (every source row appears as its
//   own ancestor).
// - Multi-edge multiplicity counting (`UNION ALL` inside the
//   recursive term + `GROUP BY` in the outer SELECT).
// - Idempotency on rerun (`ON CONFLICT … DO UPDATE SET path_count =
//   EXCLUDED.path_count` REPLACES, never adds).
// - `with_max_depth` truncation.
// - `with_roots(...)` subset walk.
// - Zero-self-FK descriptive error.
//
// Each test uses `#[djogi::djogi_test(sync_models = [...])]` for
// per-database isolation. Closure-table uniqueness is declared through model
// metadata so `materialize_closure` can exercise its typed `ON CONFLICT`
// contract without setup DDL in the test body.
//
// # Why `phase8_closure_*` and not `phase8_tree_*`
//
// The closure source-models intentionally use a different table-name
// prefix from the tree-query live tests. Each `#[djogi_test]` gets a
// per-test database, so cross-file table collisions do not happen at
// runtime; the prefix split is defensive against the inventory-side
// descriptor registry that *is* process-global, so two `#[model]`
// invocations against the same table-name string would warn at
// startup.

use djogi::prelude::*;

// ── Single-edge tree (parent_id) ────────────────────────────────────────────

#[model(table = "phase8_closure_tree_node", pk = HeerId, tree_edge = "parent_id")]
#[derive(Debug, Clone)]
pub struct ClosureTreeNode {
    pub name: String,
    pub parent_id: Option<ForeignKey<ClosureTreeNode>>,
}

#[model(
    table = "phase8_closure_tree_node_closure",
    pk = HeerId,
    no_default,
    indexes(unique(fields = [tree_node_id, ancestor_id, depth]))
)]
#[derive(Debug, Clone)]
pub struct ClosureTreeNodeAncestry {
    pub tree_node_id: ForeignKey<ClosureTreeNode>,
    pub ancestor_id: ForeignKey<ClosureTreeNode>,
    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for ClosureTreeNodeAncestry {
    type Source = ClosureTreeNode;
    fn source_column() -> &'static str {
        "tree_node_id"
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

// ── Multi-edge pedigree (mother_id + father_id) ────────────────────────────

#[model(table = "phase8_closure_pedigree", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ClosurePedigree {
    pub name: String,
    pub mother_id: Option<ForeignKey<ClosurePedigree>>,
    pub father_id: Option<ForeignKey<ClosurePedigree>>,
}

#[model(
    table = "phase8_closure_pedigree_ancestry",
    pk = HeerId,
    no_default,
    indexes(unique(fields = [pedigree_id, ancestor_id, depth]))
)]
#[derive(Debug, Clone)]
pub struct ClosurePedigreeAncestry {
    pub pedigree_id: ForeignKey<ClosurePedigree>,
    pub ancestor_id: ForeignKey<ClosurePedigree>,
    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for ClosurePedigreeAncestry {
    type Source = ClosurePedigree;
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

// ── Zero-self-FK source (negative case) ────────────────────────────────────

#[model(table = "phase8_closure_orphan", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ClosureOrphan {
    pub name: String,
}

#[model(
    table = "phase8_closure_orphan_ancestry",
    pk = HeerId,
    no_default,
    indexes(unique(fields = [orphan_id, ancestor_id, depth]))
)]
#[derive(Debug, Clone)]
pub struct ClosureOrphanAncestry {
    pub orphan_id: ForeignKey<ClosureOrphan>,
    pub ancestor_id: ForeignKey<ClosureOrphan>,
    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for ClosureOrphanAncestry {
    type Source = ClosureOrphan;
    fn source_column() -> &'static str {
        "orphan_id"
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

// ── Seed helpers ────────────────────────────────────────────────────────────

async fn seed_tree(
    ctx: &mut DjogiContext,
    name: &str,
    parent: Option<&ClosureTreeNode>,
) -> ClosureTreeNode {
    ClosureTreeNode::create(
        ctx,
        ClosureTreeNode {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            name: name.into(),
            parent_id: parent.map(|p| ForeignKey::new(p.id)),
        },
    )
    .await
    .expect("seed_tree")
}

async fn seed_pedigree_node(
    ctx: &mut DjogiContext,
    name: &str,
    mother: Option<&ClosurePedigree>,
    father: Option<&ClosurePedigree>,
) -> ClosurePedigree {
    ClosurePedigree::create(
        ctx,
        ClosurePedigree {
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

/// Fetch every row of the closure table for `phase8_closure_tree_node`,
/// returning `(tree_node_id, ancestor_id, depth, path_count)` tuples
/// sorted for deterministic comparison.
async fn closure_rows_tree(ctx: &mut DjogiContext) -> Vec<(i64, i64, i32, i64)> {
    let rows = ClosureTreeNodeAncestry::objects()
        .fetch_all(ctx)
        .await
        .expect("closure read");
    let mut rows: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.tree_node_id.key().as_i64(),
                r.ancestor_id.key().as_i64(),
                r.depth,
                r.path_count,
            )
        })
        .collect();
    rows.sort();
    rows
}

/// Fetch every row of the pedigree closure table, returning
/// `(pedigree_id, ancestor_id, depth, path_count)` tuples.
async fn closure_rows_pedigree(ctx: &mut DjogiContext) -> Vec<(i64, i64, i32, i64)> {
    let rows = ClosurePedigreeAncestry::objects()
        .fetch_all(ctx)
        .await
        .expect("pedigree closure read");
    let mut rows: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.pedigree_id.key().as_i64(),
                r.ancestor_id.key().as_i64(),
                r.depth,
                r.path_count,
            )
        })
        .collect();
    rows.sort();
    rows
}

// ── 1. Self-pairs at depth 0 ───────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [ClosureTreeNode, ClosureTreeNodeAncestry])]
async fn closure_populates_self_pairs_at_depth_zero(mut ctx: DjogiContext) {
    let root = seed_tree(&mut ctx, "root", None).await;
    let l1 = seed_tree(&mut ctx, "l1", Some(&root)).await;
    let l2 = seed_tree(&mut ctx, "l2", Some(&l1)).await;

    let report = ClosureTreeNode::materialize_closure::<ClosureTreeNodeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize_closure");
    assert_eq!(report.sources_visited, 3, "every source row visited");

    let rows = closure_rows_tree(&mut ctx).await;
    // Each of the 3 source rows must have a (self, self, 0, 1) row in
    // the closure table — that is the anchor's contribution to the
    // recursive CTE.
    for src in [root.id, l1.id, l2.id] {
        let src_int = src.as_i64();
        assert!(
            rows.iter()
                .any(|(s, a, d, c)| *s == src_int && *a == src_int && *d == 0 && *c == 1),
            "self-pair (self, self, 0, 1) missing for id={src_int}; rows={rows:?}"
        );
    }
}

// ── 2. Two edges record distinct path counts ───────────────────────────────

#[djogi::djogi_test(sync_models = [ClosurePedigree, ClosurePedigreeAncestry])]
async fn closure_two_edges_records_distinct_paths(mut ctx: DjogiContext) {
    // Linebreeding pedigree where `common` is reachable via TWO paths:
    //   common → mom (mother) → child (mother)
    //   common → dad (father) → child (father)
    let common = seed_pedigree_node(&mut ctx, "common", None, None).await;
    let mom = seed_pedigree_node(&mut ctx, "mom", Some(&common), None).await;
    let dad = seed_pedigree_node(&mut ctx, "dad", None, Some(&common)).await;
    let child = seed_pedigree_node(&mut ctx, "child", Some(&mom), Some(&dad)).await;

    let report = ClosurePedigree::materialize_closure::<ClosurePedigreeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("materialize");
    assert_eq!(report.sources_visited, 4);

    let rows = closure_rows_pedigree(&mut ctx).await;
    // (child → common, depth=2, path_count=2): two paths, one through
    // each grandparent edge sequence.
    let common_count = rows
        .iter()
        .find(|(s, a, d, _)| *s == child.id.as_i64() && *a == common.id.as_i64() && *d == 2)
        .map(|(_, _, _, c)| *c)
        .expect("(child, common, 2) row exists");
    assert_eq!(
        common_count, 2,
        "common ancestor must record path_count=2 (mother-mother and father-father paths); rows={rows:?}"
    );
}

// ── 3. Idempotent on rerun ─────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [ClosureTreeNode, ClosureTreeNodeAncestry])]
async fn closure_idempotent_on_rerun(mut ctx: DjogiContext) {
    let root = seed_tree(&mut ctx, "root", None).await;
    let _l1 = seed_tree(&mut ctx, "l1", Some(&root)).await;
    let _l2 = seed_tree(&mut ctx, "l2", Some(&_l1)).await;

    ClosureTreeNode::materialize_closure::<ClosureTreeNodeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("first run");
    let after_first = closure_rows_tree(&mut ctx).await;

    ClosureTreeNode::materialize_closure::<ClosureTreeNodeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await
    .expect("second run");
    let after_second = closure_rows_tree(&mut ctx).await;

    // Exact equality between runs — `ON CONFLICT … DO UPDATE SET
    // path_count = EXCLUDED.path_count` REPLACES, never adds. An
    // additive merge would double every path_count on rerun.
    assert_eq!(
        after_first, after_second,
        "closure must be exactly idempotent on rerun:\n\
         first  = {after_first:?}\n\
         second = {after_second:?}",
    );
}

// ── 4. with_max_depth truncates the closure ────────────────────────────────

#[djogi::djogi_test(sync_models = [ClosureTreeNode, ClosureTreeNodeAncestry])]
async fn closure_max_depth_truncates(mut ctx: DjogiContext) {
    // root → l1 → l2 → l3.
    let root = seed_tree(&mut ctx, "root", None).await;
    let l1 = seed_tree(&mut ctx, "l1", Some(&root)).await;
    let l2 = seed_tree(&mut ctx, "l2", Some(&l1)).await;
    let _l3 = seed_tree(&mut ctx, "l3", Some(&l2)).await;

    ClosureTreeNode::materialize_closure::<ClosureTreeNodeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default().with_max_depth(2),
    )
    .await
    .expect("bounded run");

    let rows = closure_rows_tree(&mut ctx).await;
    let max_depth = rows.iter().map(|(_, _, d, _)| *d).max().unwrap_or(0);
    assert!(
        max_depth <= 2,
        "with_max_depth(2) must produce no rows at depth 3+: rows={rows:?}"
    );
}

// ── 5. with_roots walks only the requested subset ─────────────────────────

#[djogi::djogi_test(sync_models = [ClosureTreeNode, ClosureTreeNodeAncestry])]
async fn closure_with_roots_walks_subset(mut ctx: DjogiContext) {
    let root = seed_tree(&mut ctx, "root", None).await;
    let l1 = seed_tree(&mut ctx, "l1", Some(&root)).await;
    let l2 = seed_tree(&mut ctx, "l2", Some(&l1)).await;
    let _other = seed_tree(&mut ctx, "other", None).await;

    let report = ClosureTreeNode::materialize_closure::<ClosureTreeNodeAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default().with_roots(vec![l1.id, l2.id]),
    )
    .await
    .expect("subset run");
    assert_eq!(
        report.sources_visited, 2,
        "with_roots subset must touch exactly the named source rows"
    );

    let rows = closure_rows_tree(&mut ctx).await;
    let visited: std::collections::HashSet<i64> = rows.iter().map(|(s, _, _, _)| *s).collect();
    assert!(
        visited.contains(&l1.id.as_i64()),
        "l1 must appear as a source: rows={rows:?}"
    );
    assert!(
        visited.contains(&l2.id.as_i64()),
        "l2 must appear as a source: rows={rows:?}"
    );
    assert!(
        !visited.contains(&root.id.as_i64()),
        "root must NOT appear as a source — not in roots: rows={rows:?}"
    );
}

// ── 6. Zero self-FKs errors descriptively ─────────────────────────────────

#[djogi::djogi_test(sync_models = [ClosureOrphan, ClosureOrphanAncestry])]
async fn closure_zero_edges_errors_descriptively(mut ctx: DjogiContext) {
    // Insert one orphan row so the source table is non-empty — the
    // self-FK guard fires before the SQL builder, so a populated
    // table still errors when the source model has no self-FK.
    let _orphan = ClosureOrphan::create(
        &mut ctx,
        ClosureOrphan {
            id: <HeerId as PrimaryKey>::sentinel(),
            created_at: DateTime::UNIX_EPOCH,
            updated_at: DateTime::UNIX_EPOCH,
            name: "lone".into(),
        },
    )
    .await
    .expect("seed orphan");

    let result = ClosureOrphan::materialize_closure::<ClosureOrphanAncestry>(
        &mut ctx,
        djogi::query::MaterializeClosureOptions::default(),
    )
    .await;

    let err = result.expect_err("zero self-FKs must error");
    let msg = err.to_string();
    assert!(
        msg.contains("phase8_closure_orphan"),
        "error must name the source model's table: {msg}"
    );
    assert!(
        msg.contains("self-FK"),
        "error must explain the self-FK requirement: {msg}"
    );
}
