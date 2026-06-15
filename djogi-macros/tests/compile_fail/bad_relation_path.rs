use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "owners_badrp")]
#[derive(Debug, Clone)]
pub struct Owner {
 pub name: String,
}

#[model(table = "vehicles_badrp", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
 pub make: String,
 pub owner_id: ForeignKey<Owner>,
}

fn main() {
 let _ = Vehicle::objects().prefetch(VehicleRelated::wrong_method());
}
