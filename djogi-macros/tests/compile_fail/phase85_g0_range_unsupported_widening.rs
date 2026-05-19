// Phase 8.5 G0 Range<T> substrate: unsigned widening scalar support does
// not imply range-subtype support.
//
// `u32` lowers to BIGINT as a scalar field, but `Range<u32>` must not
// silently become `int8range`: there is no `RangeSubtype` impl for `u32`.

use djogi::prelude::*;

#[model(
    table = "phase85_g0_range_unsupported_widening",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone)]
pub struct UnsupportedWideningRange {
    pub bad: Range<u32>,
}

fn main() {}
