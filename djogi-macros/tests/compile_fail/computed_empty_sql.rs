// Compile-fail fixture for `#[computed(sql = "")]`.
//
// Empty SQL strings are silent-no-op surfaces; rejected at parse time
// with a span-precise error pointing at the empty literal.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture rustc invocation produces a linkable artifact.

use djogi::prelude::*;

#[model(table = "phase8_computed_empty_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
 pub base_price: f64,
 #[computed(sql = "")]
 pub empty: f64,
}

fn main() {}
