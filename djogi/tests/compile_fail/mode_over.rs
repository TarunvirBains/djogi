//! Compile-fail fixture for #89: `MODE()` is an ordered-set aggregate
//! and must not expose `.over(...)`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_mode_over", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ModeOverModel {
 pub payment_method: i64,
}

fn main() {
 let _ = ModeOverModel::objects().annotate(|f| f.payment_method().mode().over(|w| w));
}
