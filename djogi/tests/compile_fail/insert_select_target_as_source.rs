//! Pin the BLOCKER fix for djogi#106: the typed INSERT...SELECT surface
//! must reject a column mapping whose source operand is built from a
//! TARGET field (`target.col().as_insert_source()`), even when the
//! target field is being copied INTO (`target.other_col().copy_from(...)`).
//!
//! The mapping `target.col1().copy_from(target.col2().as_insert_source())`
//! produces `InsertSelectColumn<TargetModel, TargetModel>`. The closure
//! must return `IntoInsertColumns<SourceModel, TargetModel>` when the
//! source `S != T`, so the doubly-target-tagged column does not match.
use djogi::prelude::*;

#[model(table = "is_target_src_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct TargetAsSourceSource {
    pub label: String,
}

#[model(table = "is_target_src_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct TargetAsSourceTarget {
    pub label: String,
    pub other: String,
}

fn main() {
    // `target.label().as_insert_source()` is the misuse — it tags the
    // operand with the TARGET model. `target.other().copy_from(...)`
    // then returns `InsertSelectColumn<TargetAsSourceTarget, TargetAsSourceTarget>`.
    // The closure expects `IntoInsertColumns<TargetAsSourceSource, TargetAsSourceTarget>`,
    // so the wrong source tag is rejected.
    let _stmt = TargetAsSourceSource::objects()
        .insert_into::<TargetAsSourceTarget, _, _>(|target, _source| {
            vec![target
                .other()
                .copy_from(target.label().as_insert_source())]
        });
}
