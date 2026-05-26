//! The typed MERGE INTO surface must reject a column mapping whose target
//! field comes from the SOURCE model and whose source operand comes
//! from the TARGET model.
//!
//! The reversed mapping `source.col().merge_copy_from(target.col())`
//! produces `MergeUpdateAssignment<TargetModel, SourceModel>`. The closure
//! must return `IntoMergeUpdates<SourceModel, TargetModel>` (parameter
//! order is `<S, T>`). The reversed type does not satisfy that trait
//! bound — the type system rejects the mapping.
use djogi::prelude::*;
use djogi::cache::Cacheable;
use djogi::query::MergeWhenCondition;

#[model(table = "merge_wrong_side_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongSideSource {
    pub label: String,
}

#[model(table = "merge_wrong_side_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongSideTarget {
    pub label: String,
}

fn main() {
    let _stmt = WrongSideSource::objects()
        .merge_into::<WrongSideTarget, _, _>(|target, source| {
            target.label().merge_on_eq(source.label())
        })
        .when_matched_and_update(None::<MergeWhenCondition<WrongSideSource, WrongSideTarget>>, vec![
            // Reversed: target field from source, source field from target.
            WrongSideSource::fields()
                .label()
                .merge_copy_from(WrongSideTarget::fields().label())
        ]);
}
