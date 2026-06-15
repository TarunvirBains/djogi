//! Compile-fail fixture for #89: value aggregates should not expose
//! `.within_group_order_by(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_sum_within_group", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct SumWithinGroupModel {
 pub amount: i64,
}

fn main() {
 let _ = SumWithinGroupModel::objects().annotate(|f| {
  f.amount().sum().within_group_order_by(f.amount().asc())
 });
}
