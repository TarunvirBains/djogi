//! Compile-fail fixture for #89: ordered-set aggregates should not expose
//! `.over(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_percentile_over", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PercentileOverModel {
 pub score: f64,
}

fn main() {
 let _ = PercentileOverModel::objects().annotate(|f| {
  f.score().percentile_cont(0.5).over(|w| w)
 });
}
