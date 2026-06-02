// Compile-fail fixture for `#[djogi::trait_impl]` on an
// inherent (non-trait) impl block. Inherent impls cannot register
// for cross-type queries; the macro rejects with an actionable
// diagnostic instructing the adopter to rewrite as a trait impl.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture rustc invocation produces a linkable artifact.

use djogi::prelude::*;

#[model(table = "phase8_trait_impl_inherent_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub title: String,
}

#[djogi::trait_impl]
impl Vehicle {
    fn helper(&self) {}
}

fn main() {}
