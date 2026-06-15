//! E_DJG_VDF_001: missing required `#[derived(...)]`
//! attribute key.
//!
//! Pins the parse-time required-key check in
//! `djogi-macros/src/model/derived.rs::parse_one`. The required keys
//! are `name`, `ty`, `scopes`, `sql`, and `rust`; this fixture omits
//! `name` and asserts the diagnostic anchors at the attribute span
//! with the E_DJG_VDF_001 code in the message.

use djogi::prelude::*;

#[model(table = "phase85_e001_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 ty  = String,
 scopes = [public],
 sql = "inbound_site",
 rust = "model.inbound_site.clone()",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
}

fn main() {}
