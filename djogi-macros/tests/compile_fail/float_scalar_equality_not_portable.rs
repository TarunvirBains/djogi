// SQL-bindable float fields are not portable
// equality fields.
//
// PostgreSQL treats NaN equality differently from Rust/Punnu, so direct
// portable equality/membership must not be available on scalar float model
// fields. SQL-only comparisons remain reachable through
// `.explicit_pg_predicate()`.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has `fn main`
// so `.stderr` does not pick up E0601 noise.

use djogi::prelude::*;

#[model(table = "phase8eta_float_scalar_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub score: f64,
}

fn main() {
    let _bad = Widget::objects().filter(|f| f.score().eq(f64::NAN));
}
