// Phase 8.5 G0 Range<T> substrate: std/core range wrappers are not
// Djogi runtime-backed range columns.
//
// The macro must not lower `std::ops::Range<T>` or `core::ops::Range<T>`
// to `FieldSqlType::Range`; they should fall through to the field-site
// `DjogiSqlType` surface and fail there.

use djogi::prelude::*;

#[model(
    table = "phase85_g0_range_unsupported_std_core_outer",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone)]
pub struct UnsupportedStdCoreOuterRange {
    pub std_ops: std::ops::Range<i32>,
    pub core_ops: core::ops::Range<i32>,
}

fn main() {}
