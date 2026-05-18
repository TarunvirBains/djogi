//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept the variadic metadata aggregate because it would
//! synthesize invalid `OVER ()` SQL for `GROUPING(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_grouping_of", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainGroupingOfModel {
    pub region: i64,
    pub dept: i64,
}

fn main() {
    let _ = PlainGroupingOfModel::objects().annotate(|_| djogi::grouping_of(&["region", "dept"]));
}
