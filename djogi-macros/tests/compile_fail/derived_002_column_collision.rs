//! E_DJG_VDF_002: derived `name` collides with an
//! exposed model column in any overlapping scope.
//!
//! `cross_check` in `djogi-macros/src/model/derived.rs` walks the
//! parsed derived attrs against the host model's column exposures
//! (`FieldAttrs::expose`) and rejects when the derived `name` matches
//! a column exposed in any of the derived entry's `scopes`.
//!
//! This fixture pins the rejection: the column `inbound_site` is
//! exposed in scope `public`; a derived entry with `name =
//! inbound_site` in the same scope must be rejected at parse time.

use djogi::prelude::*;

#[model(table = "phase85_e002_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = inbound_site,
 ty  = String,
 scopes = [public],
 sql = "'X'",
 rust = "String::from(\"X\")",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
}

fn main() {}
