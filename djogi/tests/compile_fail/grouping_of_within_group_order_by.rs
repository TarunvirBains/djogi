//! Compile-fail fixture for #89: the variadic `grouping_of(...)` form
//! is metadata-kind and must not expose `.within_group_order_by(...)`
//! (which lives only on ordered-set / hypothetical-set kinds).
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_grouping_of_within_group_order_by", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct GroupingOfWithinGroupOrderByModel {
 pub region: i64,
}

fn main() {
 let _ = GroupingOfWithinGroupOrderByModel::objects().annotate(|f| {
  djogi::grouping_of(&["region", "dept"]).within_group_order_by(f.region().asc())
 });
}
