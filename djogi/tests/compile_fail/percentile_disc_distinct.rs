//! Compile-fail fixture for #89: `PERCENTILE_DISC` is an ordered-set
//! aggregate and must not expose `.distinct()`. Sibling coverage to
//! `percentile_cont_distinct.rs`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_percentile_disc_distinct", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PercentileDiscDistinctModel {
    pub score: f64,
}

fn main() {
    let _ = PercentileDiscDistinctModel::objects().annotate(|f| {
        f.score().percentile_disc(0.5).distinct()
    });
}
