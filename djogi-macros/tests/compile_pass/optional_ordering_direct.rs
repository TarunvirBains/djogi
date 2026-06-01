// #107 - direct ordering on `DjogiField<M, Option<U>>` accepts
// inner scalar values and applies SQL-compatible NULL exclusion semantics.
//
// This used to require `.some().gt(value)`. keeps `.some()` as an
// explicit route, but also permits direct inner-value ordering for the common
// case where NULL rows should not match the comparison.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has `fn main`
// so `.stderr` does not pick up E0601 noise.

use djogi::prelude::*;

#[model(table = "phase8eta_opt_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub estimated_year: Option<i16>,
}

fn main() {
    let _direct = Widget::objects().filter(|f| f.estimated_year().gt(2020));
    let _explicit = Widget::objects().filter(|f| f.estimated_year().some().gt(2020));
}
