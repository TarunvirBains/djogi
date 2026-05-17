//! Phase 8.5 #231 — E_DJG_VDF_013: `scopes = [...]` list contains a
//! duplicate scope identifier.
//!
//! `parse_scopes_value` in `djogi-macros/src/model/derived.rs`
//! rejects per-list duplicates at parse time even though the
//! post-parse collation would deduplicate them. Rejecting at parse
//! time keeps the declaration honest and catches copy-paste bugs
//! before the macro emits any tokens. The diagnostic anchors at the
//! second occurrence of the duplicated identifier.

use djogi::prelude::*;

#[model(table = "phase85_e013_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = facility_site,
    ty     = String,
    scopes = [public, public],
    sql    = "inbound_site",
    rust   = "model.inbound_site.clone()",
)]
pub struct Consignment {
    #[field(expose(public))]
    pub inbound_site: String,
}

fn main() {}
