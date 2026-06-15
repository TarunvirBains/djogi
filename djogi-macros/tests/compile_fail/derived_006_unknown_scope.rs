//! E_DJG_VDF_006: `scopes = [...]` contains an
//! unknown scope identifier.
//!
//! `parse_scopes_value` in `djogi-macros/src/model/derived.rs`
//! checks each `scopes = [...]` element against the canonical set
//! `{public, self_view, admin, export}` via `binary_search` on a
//! sorted const slice. Anything else surfaces as E_DJG_VDF_006 at
//! parse time, anchored at the offending identifier's source span.

use djogi::prelude::*;

#[model(table = "phase85_e006_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = facility_site,
 ty  = String,
 scopes = [public, internal],
 sql = "inbound_site",
 rust = "model.inbound_site.clone()",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
}

fn main() {}
