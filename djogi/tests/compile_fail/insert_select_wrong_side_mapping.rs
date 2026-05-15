//! Pin the BLOCKER fix for djogi#106: the typed INSERT...SELECT surface
//! must reject a column mapping whose target field comes from the
//! SOURCE model and whose source operand comes from the TARGET model.
//! Pre-fix this would silently type-check whenever the two `V`s
//! matched, producing SQL that wrote the wrong rows into the wrong table.
//!
//! The reversed mapping `source.col().copy_from(target.col().as_insert_source())`
//! produces `InsertSelectColumn<TargetModel, SourceModel>`. The closure
//! must return `IntoInsertColumns<SourceModel, TargetModel>` (parameter
//! order is `<S, T>`). The reversed column-type does not satisfy that
//! trait bound — the type system rejects the mapping at the closure
//! return-type inference step.
use djogi::prelude::*;

#[model(table = "is_wrong_side_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongSideSource {
    pub label: String,
}

#[model(table = "is_wrong_side_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongSideTarget {
    pub label: String,
}

fn main() {
    // `source.label().copy_from(target.label().as_insert_source())` is
    // the reversed mapping. `source.label()` is `DjogiField<WrongSideSource, String>`,
    // its `copy_from` returns `InsertSelectColumn<X, WrongSideSource>`
    // for whatever `X` the operand carries — here `X = WrongSideTarget`.
    // The closure's required return is `Vec<InsertSelectColumn<WrongSideSource, WrongSideTarget>>`
    // (the parameter order is `<S, T>`), so the actual type
    // `Vec<InsertSelectColumn<WrongSideTarget, WrongSideSource>>` does
    // not match.
    let _stmt = WrongSideSource::objects()
        .insert_into::<WrongSideTarget, _, _>(|target, source| {
            vec![source
                .label()
                .copy_from(target.label().as_insert_source())]
        });
}
