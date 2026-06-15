//! `expose(public, public = "X")` mixes scalar and relation forms on the
//! same scope — must be rejected.
use djogi::prelude::*;

#[model(table = "vehicles_expose_mixed")]
#[derive(Debug, Clone)]
pub struct Vehicle {
 #[field(expose(public, public = "VehicleSummary"))]
 pub make: String,
}

fn main() {}
