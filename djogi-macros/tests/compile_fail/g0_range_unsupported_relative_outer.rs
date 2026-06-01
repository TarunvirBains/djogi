// G0 Range<T> substrate: crate/self/super-relative wrappers
// named `Range` are not accepted as Djogi range columns.
//
// Only the actual djogi runtime surface (`Range` from the prelude/import,
// `djogi::Range`, `djogi::types::Range`, and leading-`::` variants) may
// lower to `FieldSqlType::Range`.

use djogi::prelude::*;

#[derive(Debug, Clone)]
pub struct Range<T>(std::marker::PhantomData<T>);

#[model(
    table = "phase85_g0_range_unsupported_crate_self_outer",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone)]
pub struct UnsupportedCrateSelfOuterRange {
    pub crate_relative: crate::Range<i32>,
    pub self_relative: self::Range<i32>,
}

mod nested {
    use super::*;

    #[model(
        table = "phase85_g0_range_unsupported_super_outer",
        pk = HeerId,
        no_default
    )]
    #[derive(Debug, Clone)]
    pub struct UnsupportedSuperOuterRange {
        pub super_relative: super::Range<i32>,
    }
}

fn main() {}
