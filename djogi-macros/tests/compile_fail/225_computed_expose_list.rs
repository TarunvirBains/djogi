// Compile-fail fixture for `expose(...)` list form
// inside `#[computed(...)]`.
//
// An adopter who copied the early Path A grammar `expose(public, admin)`
// into `#[computed(...)]` receives a hard compile error pointing at
// `#[derived(...)]` as the correct surface. The list form was a variant
// of the same Path A conflation between model-side computed columns and
// visage-side projection entries.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture
// rustc invocation produces a linkable artifact.

use djogi::prelude::*;

#[model(table = "phase85_225_expose_list_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub base_price: f64,
    #[computed(sql = "base_price * 2", expose(public, admin))]
    pub double_price: f64,
}

fn main() {}
