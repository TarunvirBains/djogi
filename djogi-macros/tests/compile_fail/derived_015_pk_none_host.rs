//! E_DJG_VDF_015: `#[derived(...)]` declared on a
//! `#[model(pk = None)]` host model.
//!
//! `cross_check` in `djogi-macros/src/model/derived.rs` rejects any
//! `#[derived(...)]` attribute on a `pk = None` model at parse time.
//! Derived visages hydrate per-row identified by primary key; a `pk
//! = None` model has no `id` column, no `Model::Pk` associated type,
//! and no visage queryset to filter against. The framework rejects
//! the combination rather than silently emitting a broken visage
//! surface.

use djogi::prelude::*;

#[model(table = "phase85_e015_consignments", pk = None)]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = facility_site,
    ty     = String,
    scopes = [public],
    sql    = "inbound_site",
    rust   = "model.inbound_site.clone()",
)]
pub struct Consignment {
    #[field(expose(public))]
    pub inbound_site: String,
}

fn main() {}
