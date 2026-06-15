use djogi::prelude::*;

#[model(table = "owners")]
#[derive(Debug, Clone)]
pub struct Owner {
 pub name: String,
}

#[model(table = "pets", no_default)]
#[derive(Debug, Clone)]
pub struct Pet {
 #[field(deferrable, initially_deferred)]
 pub owner_id: ForeignKey<Owner>,
}

fn _accepts_deferrable_fk_attrs() {
 let _ = PetFields::default().owner_id();
}

fn main() {}
