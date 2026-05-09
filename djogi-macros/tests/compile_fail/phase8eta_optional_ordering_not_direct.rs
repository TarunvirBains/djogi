// Phase 8eta PR3 — direct ordering on `DjogiField<M, Option<U>>` is
// deliberately unavailable.
//
// Rust's `Option` ordering convention (`None < Some(_)`) does NOT match
// SQL's three-valued NULL logic, where comparisons against NULL evaluate
// to NULL (excluded from the result set). Allowing a direct
// `.gt(value)` on `DjogiField<M, Option<U>>` would silently mask that
// difference: a Punnu in-memory walk would treat NULL rows one way
// while the database-backed SQL emit treated them another.
//
// PR3 makes the divergence a compile-time error. Adopters reach value
// comparisons via `.some().gt(value)` (which evaluates `None` as
// `false` in Punnu and emits SQL that excludes NULL rows) and
// nullability via `.is_null()` / `.is_not_null()`.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture has
// `fn main` so `.stderr` does not pick up E0601 noise.

use djogi::prelude::*;

#[model(table = "phase8eta_opt_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub estimated_year: Option<i16>,
}

fn main() {
    // Direct ordering on `DjogiField<Widget, Option<i16>>` is omitted.
    // The supported routes are `.some().gt(value)` (value comparison,
    // SQL NULL exclusion) and `.is_null()` / `.is_not_null()`
    // (nullness). This call must fail to compile.
    let _bad = Widget::objects().filter(|f| f.estimated_year().gt(2020));
}
