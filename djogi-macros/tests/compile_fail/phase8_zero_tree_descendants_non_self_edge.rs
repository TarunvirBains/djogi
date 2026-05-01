// Phase 8-Zero Cluster B5 (T14a) — `QuerySet::tree_descendants` requires
// a `RelationPath<T, T>` (a self-FK), not a foreign-FK relation.
//
// The compile-time guard rides on the `RelationPath<T, T>` parameter type
// in `QuerySet::tree_descendants` (see djogi/src/query/queryset.rs) and
// `RecursiveQuerySet::from_path` (djogi/src/query/recursive.rs). Passing a
// `RelationPath<Node, Owner>` (cross-model FK) yields an E0308 type
// mismatch — Source and Target both have to be the same model for a tree
// walk to make sense.
//
// `fn main() {}` per feedback_trybuild_fixtures.md.

use djogi::prelude::*;

#[model(table = "phase8_owner_b5")]
#[derive(Debug, Clone)]
pub struct OwnerB5 {
    pub name: String,
}

#[model(table = "phase8_node_b5", no_default)]
#[derive(Debug, Clone)]
pub struct NodeB5 {
    pub owner_id: ForeignKey<OwnerB5>,
    pub label: String,
}

fn main() {
    // `NodeB5Related::owner()` returns `RelationPath<NodeB5, OwnerB5>`.
    // `QuerySet::<NodeB5>::tree_descendants` requires
    // `RelationPath<NodeB5, NodeB5>`. The mismatch on the second type
    // parameter (`OwnerB5` vs `NodeB5`) surfaces as E0308.
    let id = <HeerId as PrimaryKey>::sentinel();
    let _qs = NodeB5::objects().tree_descendants(NodeB5Related::owner(), id);
}
