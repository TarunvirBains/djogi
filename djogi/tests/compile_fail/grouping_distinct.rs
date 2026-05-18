//! Compile-fail fixture for #89: `GROUPING(...)` should be
//! metadata-kind and must not expose `.distinct()`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_grouping_distinct", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct GroupingDistinctModel {
    pub region: i64,
}

fn main() {
    let _ = GroupingDistinctModel::objects().annotate(|f| f.region().grouping().distinct());
}
