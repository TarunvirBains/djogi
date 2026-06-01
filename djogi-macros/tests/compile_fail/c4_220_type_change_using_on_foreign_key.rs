// Cluster 4 djogi#220 — `#[field(type_change_using = "...")]`
// on a `ForeignKey<T>` field is rejected.
//
// FK type changes flow through the PK-flip orchestration on the
// parent model — the child column's storage type follows the parent's PK,
// and an adopter USING on the child cannot drive the typed flip
// apparatus. If the parent's PK shape is changing, the migration emitter
// routes it through the PK-flip path automatically.

use djogi::prelude::*;

#[model(table = "items_220_fk_parent", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Item220FkParent {
    pub name: String,
}

#[model(table = "items_220_fk_child", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Item220FkChild {
    #[field(type_change_using = "owner_id::BIGINT")]
    pub owner: ForeignKey<Item220FkParent>,
}

fn main() {}
