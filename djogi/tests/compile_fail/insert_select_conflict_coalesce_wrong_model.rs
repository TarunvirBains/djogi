use djogi::cache::Cacheable;
use djogi::prelude::*;

#[model(table = "conflict_coalesce_wrong_model_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ConflictCoalesceWrongModelSource {
    pub slug: String,
    pub maybe_hits: Option<i32>,
}

#[model(table = "conflict_coalesce_wrong_model_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ConflictCoalesceWrongModelTarget {
    pub slug: String,
    pub maybe_hits: Option<i32>,
}

#[model(table = "conflict_coalesce_wrong_model_other_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ConflictCoalesceWrongModelOtherTarget {
    pub slug: String,
    pub maybe_hits: Option<i32>,
}

fn main() {
    let _stmt = ConflictCoalesceWrongModelSource::objects()
        .insert_into::<ConflictCoalesceWrongModelTarget, _, _>(|target, source| {
            vec![
                target.slug().copy_from(source.slug().as_insert_source()),
                target
                    .maybe_hits()
                    .copy_from(source.maybe_hits().as_insert_source()),
            ]
        })
        .on_conflict_do_update(
            ConflictTarget::columns([ConflictCoalesceWrongModelTarget::fields().slug()]),
            |t| {
                vec![t.maybe_hits().conflict_set_expr(
                    ConflictCoalesceWrongModelOtherTarget::fields()
                        .maybe_hits()
                        .conflict_coalesce_excluded(),
                )]
            },
        );
}
