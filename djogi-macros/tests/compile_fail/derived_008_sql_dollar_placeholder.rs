//! E_DJG_VDF_008: derived `sql` contains a `$N`
//! placeholder token outside string-literal context.
//!
//! `validate_sql_surface` in `djogi-macros/src/model/derived.rs`
//! invokes `contains_unquoted_dollar_digit` which detects a `$`
//! followed by one or more ASCII digits at any unquoted position.
//! `$1`, `$2`, etc. are reserved for future cross-model references
//! and cannot appear in derived expressions in v0.1.0.

use djogi::prelude::*;

#[model(table = "phase85_e008_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = facility_site,
 ty  = String,
 scopes = [public],
 sql = "COALESCE(inbound_site, $1)",
 rust = "model.inbound_site.clone()",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
}

fn main() {}
