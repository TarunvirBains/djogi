//! E_DJG_VDF_007: derived `sql` contains a `;`
//! statement separator outside string-literal context.
//!
//! `validate_sql_surface` in `djogi-macros/src/model/derived.rs`
//! walks `sql` byte-by-byte through `contains_unquoted_byte`. A `;`
//! that is NOT inside a single-quoted string or a dollar-quoted body
//! triggers E_DJG_VDF_007 at parse time — derived expressions must
//! be a single per-row scalar.
//!
//! The leading-DDL/DML keyword case (also E_DJG_VDF_007) is the same
//! diagnostic surface but a different trigger arm; the statement-
//! separator case is the simplest to pin in a fixture.

use djogi::prelude::*;

#[model(table = "phase85_e007_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = facility_site,
 ty  = String,
 scopes = [public],
 sql = "inbound_site; SELECT 1",
 rust = "model.inbound_site.clone()",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
}

fn main() {}
