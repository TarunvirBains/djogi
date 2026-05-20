// Phase 8.5 #225 — Compile-fail fixture for `expose = "..."` inside
// `#[computed(...)]`.
//
// The assignment form `#[computed(sql = "...", expose = "public")]` was
// entertained in an early Path A draft that conflated model-side virtual
// columns with visage-side projection entries. That design was rejected:
// `#[computed(sql = "...")]` is a model-side surface only; it never
// declares visage exposure. Visage exposure is declared via the
// struct-level `#[derived(name, ty, scopes, sql, rust)]` attribute.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture
// rustc invocation produces a linkable artifact.

use djogi::prelude::*;

#[model(table = "phase85_225_expose_asgn_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub base_price: f64,
    #[computed(sql = "base_price * 2", expose = "public")]
    pub double_price: f64,
}

fn main() {}
