//! Compile-fail fixture for #89: `MODE()` is an ordered-set aggregate
//! and must not expose in-paren `.order_by(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_mode_order_by", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ModeOrderByModel {
 pub payment_method: i64,
}

fn main() {
 let _ = ModeOrderByModel::objects().annotate(|f| {
  f.payment_method().mode().order_by(f.payment_method().asc())
 });
}
