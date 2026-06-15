//! Compile-fail fixture for #89: the variadic `grouping_of(...)` form
//! is metadata-kind and must not expose `.order_by(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_grouping_of_order_by", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct GroupingOfOrderByModel {
 pub region: i64,
}

fn main() {
 let _ = GroupingOfOrderByModel::objects().annotate(|f| {
  djogi::grouping_of(&["region", "dept"]).order_by(f.region().asc())
 });
}
