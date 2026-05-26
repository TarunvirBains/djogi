//! `MERGE` expression assignments must not accept source-field expressions
//! after they have been erased into the generic `Expr<V>` tree.
//!
//! `FieldRef::as_expr()` intentionally drops the model identity for normal
//! single-query expression composition. In MERGE, that is unsafe because
//! target and source columns need different SQL aliases. The merge expression
//! surface therefore accepts target field handles, not generic source-originated
//! `Expr<V>` values.
use djogi::prelude::*;
use djogi::cache::Cacheable;
use djogi::query::MergeWhenCondition;

#[model(table = "merge_source_expr_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct SourceExprSource {
    pub label: String,
}

#[model(table = "merge_source_expr_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct SourceExprTarget {
    pub label: String,
}

fn main() {
    let _stmt = SourceExprSource::objects()
        .merge_into::<SourceExprTarget, _, _>(|target, source| {
            target.label().merge_on_eq(source.label())
        })
        .when_matched_and_update(
            None::<MergeWhenCondition<SourceExprSource, SourceExprTarget>>,
            SourceExprTarget::fields()
                .label()
                .merge_set_expr(SourceExprSource::fields().label().as_expr()),
        );
}
