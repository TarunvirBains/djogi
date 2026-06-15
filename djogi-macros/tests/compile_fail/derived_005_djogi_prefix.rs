//! E_DJG_VDF_005: derived `name` uses the
//! framework-reserved `__djogi_` prefix.
//!
//! `validate_name_shape` in `djogi-macros/src/model/derived.rs`
//! rejects any derived `name` whose ASCII-case-insensitive lowercase
//! lowering begins with `__djogi_`. This prefix is reserved for the
//! framework's own internal columns (e.g. `__djogi_path` on
//! `{Visage}Fields`); adopter-side derived entries must pick another
//! name.

use djogi::prelude::*;

#[model(table = "phase85_e005_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = __djogi_facility_site,
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
