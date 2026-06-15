use djogi::cache::Cacheable;
use djogi::prelude::*;

#[model(table = "conflict_null_non_option_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ConflictNullNonOptionSource {
    pub slug: String,
    pub hits: i32,
}

#[model(table = "conflict_null_non_option_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ConflictNullNonOptionTarget {
    pub slug: String,
    pub hits: i32,
}

fn main() {
    let _stmt = ConflictNullNonOptionSource::objects()
        .insert_into::<ConflictNullNonOptionTarget, _, _>(|target, source| {
            vec![
                target.slug().copy_from(source.slug().as_insert_source()),
                target.hits().copy_from(source.hits().as_insert_source()),
            ]
        })
        .on_conflict_do_update(
            ConflictTarget::columns([ConflictNullNonOptionTarget::fields().slug()]),
            |t| {
                let _guard = t.hits().conflict_is_null();
                vec![t.hits().conflict_set_expr(t.hits().conflict_coalesce_excluded())]
            },
        );
}
