// Phase 8β T3.5 — Compile-fail fixture for the T3.3 runtime-value
// rejection in `default_filter` closures.
//
// The closure body references `runtime_var` — an identifier that is
// not an inline literal. The lowering pass in
// `model::proxy::lower_default_filter_to_sql` rejects every non-
// literal RHS with a span-precise diagnostic pointing at the
// offending node and instructing the adopter to implement
// `Model::default_filter_condition` by hand for non-literal RHS.
//
// Every lihaaf compile-fixture must have
// `fn main() {}` so the stored `.stderr` does not pick up E0601.

use djogi::prelude::*;

const RUNTIME_VAR: bool = true;

#[model(table = "phase8_proxy_runtime_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub name: String,
    pub active: bool,
}

#[model(
    table = "phase8_proxy_runtime_vehicles",
    proxy_for = Vehicle,
    default_filter = |f| f.active.eq(RUNTIME_VAR),
)]
#[derive(Debug, Clone)]
pub struct ActiveVehicle {
    pub name: String,
    pub active: bool,
}

fn main() {}
