//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept hypothetical-set aggregates because it would
//! synthesize invalid `OVER ()` SQL for `DENSE_RANK(...) WITHIN GROUP`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_dense_rank_of", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainDenseRankOfModel {
    pub salary: i64,
}

fn main() {
    let _ = PlainDenseRankOfModel::objects().annotate(|f| f.salary().dense_rank_of(7_500));
}
