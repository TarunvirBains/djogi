// SQL array support is wider than portable array
// equality.
//
// `Vec<f64>` remains a supported Postgres array column and can use SQL-only
// array operators through `.explicit_pg_predicate()`, but direct portable
// equality/membership is deliberately unavailable. Rust/Punnu float equality
// and PostgreSQL float equality differ around NaN parity, so cache/refresh
// predicates must not accept this shape.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has `fn main`
// so `.stderr` does not pick up E0601 noise.

use djogi::prelude::*;

#[model(table = "phase8eta_float_array_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
 pub samples: Vec<f64>,
}

fn main() {
 let _bad = Widget::objects().filter(|f| f.samples().eq(vec![1.0, f64::NAN]));
}
