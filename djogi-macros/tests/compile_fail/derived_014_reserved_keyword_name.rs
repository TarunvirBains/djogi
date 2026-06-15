//! E_DJG_VDF_014: derived `name` is a Postgres
//! reserved keyword.
//!
//! `validate_name_shape` in `djogi-macros/src/model/derived.rs`
//! routes the captured ident through `crate::ident::check_one`'s
//! reserved-keyword arm (sorted const slice
//! `RESERVED_KEYWORDS`). A reserved keyword as a derived `name`
//! cannot appear unquoted in generated SQL aliases without breaking
//! the SELECT projection; this fixture pins the dedicated
//! E_DJG_VDF_014 diagnostic (separate from the general shape rule
//! E_DJG_VDF_004 so the prose can name the keyword conflict).

use djogi::prelude::*;

#[model(table = "phase85_e014_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = select,
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
