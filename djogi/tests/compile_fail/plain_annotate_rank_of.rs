//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept hypothetical-set aggregates because it would
//! synthesize invalid `OVER ()` SQL for `RANK(...) WITHIN GROUP`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_rank_of", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainRankOfModel {
 pub salary: i64,
}

fn main() {
 let _ = PlainRankOfModel::objects().annotate(|f| f.salary().rank_of(7_500));
}
