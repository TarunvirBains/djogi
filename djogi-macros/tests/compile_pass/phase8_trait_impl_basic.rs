// Phase 8β T5.5 — Minimal compile-pass fixture for #[djogi::trait_impl].
//
// Declares a `Searchable` trait and a `Vehicle` model, and registers
// Vehicle as a Searchable provider via the attribute macro. The
// emitted impl block reaches rustc verbatim; the registration emits
// alongside via `inventory::submit!`.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture rustc invocation produces a linkable artifact.

use djogi::prelude::*;

trait Searchable {
    fn searchable_columns(&self) -> &'static [&'static str];
}

#[model(table = "phase8_trait_impl_basic_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub title: String,
}

#[djogi::trait_impl]
impl Searchable for Vehicle {
    fn searchable_columns(&self) -> &'static [&'static str] {
        &["title"]
    }
}

fn main() {}
