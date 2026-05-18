// Phase 8.5 G0 Range<T> substrate: adopter-module wrappers named
// `Range` are not Djogi runtime-backed range columns.

use djogi::prelude::*;

mod adopter {
    #[derive(Debug, Clone)]
    pub struct Range<T>(std::marker::PhantomData<T>);
}

mod alias_shadow {
    #[derive(Debug, Clone)]
    pub struct Range<T>(std::marker::PhantomData<T>);
}

#[model(
    table = "phase85_g0_range_unsupported_adopter_outer",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone)]
pub struct UnsupportedAdopterOuterRange {
    pub adopter_module: adopter::Range<i32>,
    pub alias_shadow_relative: self::alias_shadow::Range<i32>,
}

fn main() {}
