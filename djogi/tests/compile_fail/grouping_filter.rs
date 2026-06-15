//! Compile-fail fixture for #89: `GROUPING(...)` should be
//! metadata-kind and must not expose `.filter(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_grouping_filter", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct GroupingFilterModel {
 pub region: i64,
}

fn main() {
 let _ = GroupingFilterModel::objects().annotate(|f| {
  f.region().grouping().filter(Expr::literal(true))
 });
}
