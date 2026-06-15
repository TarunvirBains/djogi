//! Compile-fail fixture for #89: plain ungrouped `QuerySet::annotate`
//! must not accept ordered-set aggregates because it would synthesize
//! invalid `OVER ()` SQL for `MODE`.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_mode", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainModeModel {
 pub payment_method: i64,
}

fn main() {
 let _ = PlainModeModel::objects().annotate(|f| f.payment_method().mode());
}
