//! Multi-model adopter model crate (mirrors a real crate like
//! elephant-tracker). Two #[derive(Model)] structs in ONE crate — the
//! multi-model dead-strip question the §9.1 spike settles.
use djogi::prelude::*;

#[derive(Model)]
#[model(table = "elephants")]
pub struct Elephant {
 pub name: String,
}

#[derive(Model)]
#[model(table = "herds")]
pub struct Herd {
 pub region: String,
}
