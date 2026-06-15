use djogi::cache::Cacheable;
use djogi::prelude::*;

#[model(table = "wrong_expr_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongExprSource {
    pub hits: i32,
}

#[model(table = "wrong_expr_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongExprTarget {
    pub hits: i32,
}

#[model(table = "wrong_expr_others", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongExprOther {
    pub hits: i32,
}

fn main() {
    let _stmt = WrongExprSource::objects()
        .insert_into::<WrongExprTarget, _, _>(|target, source| {
            vec![target.hits().copy_from(source.hits().as_insert_source())]
        })
        .on_conflict_do_update(
            ConflictTarget::columns([WrongExprTarget::fields().hits()]),
            |t| vec![t.hits().conflict_set_expr(WrongExprOther::fields().hits().as_expr())],
        );
}
