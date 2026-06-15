//! E_DJG_VDF_003: two derived entries share a
//! `name` in an overlapping scope.
//!
//! `cross_check` in `djogi-macros/src/model/derived.rs` walks the
//! parsed derived attrs pairwise; when two derived attrs share a
//! `name` and their `scopes` overlap, the second is rejected at
//! parse time.
//!
//! This fixture pins the rejection: two `#[derived(name =
//! facility_site, scopes = [public],...)]` attributes on one
//! model.

use djogi::prelude::*;

#[model(table = "phase85_e003_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
 name = facility_site,
 ty  = String,
 scopes = [public],
 sql = "inbound_site",
 rust = "model.inbound_site.clone()",
)]
#[derived(
 name = facility_site,
 ty  = String,
 scopes = [public],
 sql = "outbound_site",
 rust = "model.outbound_site.clone()",
)]
pub struct Consignment {
 #[field(expose(public))]
 pub inbound_site: String,
 #[field(expose(public))]
 pub outbound_site: String,
}

fn main() {}
