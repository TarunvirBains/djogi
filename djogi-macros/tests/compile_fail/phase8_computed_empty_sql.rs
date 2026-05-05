// Phase 8β T4.6 — Compile-fail fixture for `#[computed(sql = "")]`.
//
// Empty SQL strings are silent-no-op surfaces; rejected at parse time
// with a span-precise error pointing at the empty literal.
//
// Per `feedback_trybuild_fixtures.md`, every fixture has `fn main() {}`.

use djogi::prelude::*;

#[model(table = "phase8_computed_empty_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub base_price: f64,
    #[computed(sql = "")]
    pub empty: f64,
}

fn main() {}
