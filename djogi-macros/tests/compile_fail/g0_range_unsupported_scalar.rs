// G0 Range<T> substrate: non-range scalar elements are rejected
// at the field declaration via the `DjogiSqlType` fallback.

use djogi::prelude::*;

#[model(
    table = "phase85_g0_range_unsupported_scalar",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone)]
pub struct UnsupportedScalarRange {
    pub bad_string: Range<String>,
    pub bad_bool: Range<bool>,
}

fn main() {}
