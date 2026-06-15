//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept hypothetical-set aggregates because it would
//! synthesize invalid `OVER ()` SQL for `CUME_DIST(...) WITHIN GROUP`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_cume_dist_of", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainCumeDistOfModel {
 pub score: f64,
}

fn main() {
 let _ = PlainCumeDistOfModel::objects().annotate(|f| f.score().cume_dist_of(0.5));
}
