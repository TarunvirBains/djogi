// G0 Range<T> substrate: PK-family elements are not range
// subtypes.
//
// `HeerId` lowers to BIGINT as a scalar field, but `Range<HeerId>` must
// not silently become `int8range`: the runtime range codec only supports
// elements that implement `RangeSubtype`. Falling through to
// `DjogiSqlType` keeps the rejection at the field declaration.

use djogi::prelude::*;

#[model(
    table = "phase85_g0_range_unsupported_pk_family",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone)]
pub struct UnsupportedPkRange {
    pub bad: Range<HeerId>,
}

fn main() {}
