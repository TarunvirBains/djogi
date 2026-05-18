//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept hypothetical-set aggregates because it would
//! synthesize invalid `OVER ()` SQL for `PERCENT_RANK(...) WITHIN GROUP`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_percent_rank_of", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainPercentRankOfModel {
    pub score: f64,
}

fn main() {
    let _ = PlainPercentRankOfModel::objects().annotate(|f| f.score().percent_rank_of(0.5));
}
