//! Compile-fail fixture for #89: a tuple containing any non-windowable
//! aggregate kind must be rejected by plain ungrouped `QuerySet::annotate`,
//! even when the tuple also contains a value aggregate.
use djogi::prelude::*;

#[model(table = "phase85_aggregate89_plain_mixed_tuple", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct PlainMixedTupleModel {
 pub score: f64,
}

fn main() {
 let _ = PlainMixedTupleModel::objects()
 .annotate(|f| (f.score().sum(), f.score().percentile_cont(0.5)));
}
