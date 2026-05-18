//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept ordered-set aggregates because it would synthesize
//! invalid `OVER ()` SQL for `PERCENTILE_CONT`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_percentile_cont", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainPercentileContModel {
    pub score: f64,
}

fn main() {
    let _ = PlainPercentileContModel::objects().annotate(|f| f.score().percentile_cont(0.5));
}
