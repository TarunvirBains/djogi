//! Compile-fail fixture for #89: `GROUPING(...)` should be
//! metadata-kind and must not expose `.over(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_grouping_over", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct GroupingOverModel {
 pub region: i64,
}

fn main() {
 let _ = GroupingOverModel::objects().annotate(|f| f.region().grouping().over(|w| w));
}
