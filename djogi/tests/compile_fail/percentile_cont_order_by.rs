//! Compile-fail fixture for #89: ordered-set aggregates should not expose
//! in-paren `.order_by(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_percentile_order_by", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PercentileOrderByModel {
    pub score: f64,
}

fn main() {
    let _ = PercentileOrderByModel::objects().annotate(|f| {
        f.score().percentile_cont(0.5).order_by(f.score().asc())
    });
}
