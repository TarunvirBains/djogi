// Phase 8-Zero Cluster B5 (T14a) — A model with two self-FKs and NO
// `#[model(tree_edge = "...")]` declaration COMPILES FINE.
//
// `tree_edge` is sugar — it makes `Model::tree_descendants(id)` and
// `Model::tree_ancestors(id)` available as inherent shortcuts. Without
// `tree_edge`, those methods still exist (default trait impls) but they
// surface a runtime `DjogiError::Validation` because the descriptor's
// `tree_edge` slot is `None`.
//
// What still works without `tree_edge`:
//
//   - `QuerySet::tree_descendants(NodeRelated::mother(), id)` — explicit
//     path API, picks the edge by name.
//   - `QuerySet::tree_ancestors(NodeRelated::father(), id)` — same.
//   - `Pedigree::full_ancestors(id)` — walks BOTH self-FK edges via
//     UNION ALL; this is the actual reason a pedigree model has two
//     self-FKs in the first place.
//
// This fixture is a *compile-pass* (not compile-fail) on purpose: the
// macro must NOT reject a multi-self-FK model just because `tree_edge`
// is absent. We assert at compile time that the macro accepts the
// declaration; the runtime error path on `Model::tree_descendants(id)`
// for this shape is covered by integration tests in B5.

use djogi::prelude::*;

#[model(table = "phase8_pedigrees")]
#[derive(Debug, Clone)]
pub struct Pedigree {
    pub name: String,
    pub mother_id: Option<ForeignKey<Pedigree>>,
    pub father_id: Option<ForeignKey<Pedigree>>,
}

fn main() {
    // The model compiles. Confirm at type-level that the explicit-path
    // API and the multi-edge `full_ancestors` sugar are reachable on
    // the descriptor — both routes exist regardless of `tree_edge`.
    let id = <HeerId as PrimaryKey>::sentinel();
    let _by_mother = Pedigree::objects().tree_descendants(PedigreeRelated::mother(), id);
    let _by_father = Pedigree::objects().tree_ancestors(PedigreeRelated::father(), id);
    let _all = Pedigree::full_ancestors(id);
}
