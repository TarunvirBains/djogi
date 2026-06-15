//! Using bare-scope form `expose(public)` on a relation field must be
//! rejected — relation fields require an explicit peer projection name so
//! raw models never leak into transport projections.
use djogi::prelude::*;

#[model(table = "owners_expose_sform")]
#[derive(Debug, Clone)]
pub struct Owner {
 pub name: String,
}

#[model(table = "vehicles_expose_sform", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
 #[field(expose(public))]
 pub owner_id: ForeignKey<Owner>,
}

fn main() {}
