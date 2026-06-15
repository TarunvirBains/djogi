//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept ordered-set aggregates because it would synthesize
//! invalid `OVER ()` SQL for `PERCENTILE_DISC`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_percentile_disc", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainPercentileDiscModel {
 pub score: f64,
}

fn main() {
 let _ = PlainPercentileDiscModel::objects().annotate(|f| f.score().percentile_disc(0.5));
}
