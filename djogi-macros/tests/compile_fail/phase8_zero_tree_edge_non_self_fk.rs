// Phase 8-Zero Cluster B5 (T14a) — `#[model(tree_edge = "...")]` value
// must name a *self-FK* field, not a foreign-FK pointing at another model.
//
// The macro validates the column kind at expansion time: a column whose
// `ForeignKey<T>` target is not `Self` cannot anchor a tree-recursive walk.
// The error is span-precise (points at the literal) and explicitly names
// both the model and the requirement that the FK target match the source.
//
// `fn main() {}` per feedback_trybuild_fixtures.md.

use djogi::prelude::*;

#[model(table = "phase8_owners")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "phase8_non_self_fk_nodes", tree_edge = "owner_id")]
#[derive(Debug, Clone)]
pub struct NonSelfFkNode {
    pub owner_id: ForeignKey<Owner>,
    pub label: String,
}

fn main() {}
