//! Tier-1 accessor exclusion contract pin.
//!
//! Derived projection entries are intentionally EXCLUDED from the
//! generated `{Visage}Fields` accessor type in Tier 1. The exclusion
//! is mechanical (the emitter walks column entries only when
//! generating per-field accessors; derived entries are filtered out
//! in `djogi-macros/src/model/visage_fields.rs`).
//!
//! Calling `f.<derived>()` on a visage's fields ZST must therefore
//! produce a rustc "no method named ..." diagnostic. That precise
//! diagnostic is the Tier-1 contract: when Tier 2 ships, the
//! accessor surface widens to include derived fields, and that
//! widening is a deliberate spec amendment — not an accidental
//! regression. This fixture pins the current contract so the
//! widening shows up as a `.stderr` diff at review time.
//!
//! See `docs/spec/visage-derived-fields.md` §"Capability tiers" for
//! the full reasoning.

use djogi::prelude::*;

#[model(table = "phase85_derived_tier1_accessor_excluded_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = facility_site,
    ty     = String,
    scopes = [public],
    sql    = "CASE WHEN direction = 'inbound' \
                  THEN inbound_site \
                  ELSE outbound_site END",
    rust   = "if model.direction == \"inbound\" { \
                  model.inbound_site.clone() \
              } else { \
                  model.outbound_site.clone() \
              }",
)]
pub struct Consignment {
    #[field(expose(public))]
    pub inbound_site: String,
    #[field(expose(public))]
    pub outbound_site: String,
    #[field(expose(public))]
    pub direction: String,
}

fn main() {
    // The `f.facility_site()` accessor MUST NOT exist on
    // `ConsignmentPublicFields`. Storage-column accessors
    // (`f.inbound_site()`, `f.direction()`) work; the derived
    // `facility_site` does not. When Tier 2 ships, this fixture
    // must be deleted (or converted to a compile_pass).
    let _qs = ConsignmentPublic::filter(|f| {
        f.facility_site().eq(String::from("FAC-1"))
    });
}
