//! Compile-fail fixture for #89: ordered-set aggregates should not expose
//! `.distinct()`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_percentile_distinct", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PercentileDistinctModel {
    pub score: f64,
}

fn main() {
    let _ = PercentileDistinctModel::objects().annotate(|f| {
        f.score().percentile_cont(0.5).distinct()
    });
}
