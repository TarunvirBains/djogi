//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept metadata aggregates because it would synthesize
//! invalid `OVER ()` SQL for `GROUPING`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_grouping", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainGroupingModel {
    pub region: i64,
}

fn main() {
    let _ = PlainGroupingModel::objects().annotate(|f| f.region().grouping());
}
