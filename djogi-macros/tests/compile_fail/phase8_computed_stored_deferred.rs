// Phase 8β T4.6 — Compile-fail fixture for `#[computed(sql = "...", stored)]`.
//
// The `stored` keyword is **always rejected** at parse time with a
// Phase 8.5 deferral message per `feedback_anchored_deferrals` — the
// migration differ has not yet accumulated long-running stability
// evidence post-publish, so generating column DDL from
// `#[computed(stored)]` is out of scope for v0.1.0.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture rustc invocation produces a linkable artifact.

use djogi::prelude::*;

#[model(table = "phase8_computed_stored_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub base_price: f64,
    #[computed(sql = "base_price * 2", stored)]
    pub stored_double_price: f64,
}

fn main() {}
