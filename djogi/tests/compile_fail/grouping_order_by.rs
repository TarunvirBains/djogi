//! Compile-fail fixture for #89: `GROUPING(...)` should be
//! metadata-kind and must not expose `.order_by(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_grouping_order_by", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct GroupingOrderByModel {
 pub region: i64,
}

fn main() {
 let _ = GroupingOrderByModel::objects().annotate(|f| {
  f.region().grouping().order_by(f.region().asc())
 });
}
