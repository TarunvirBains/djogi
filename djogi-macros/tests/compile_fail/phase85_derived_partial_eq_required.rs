//! Phase 8.5 #231 — E_DJG_VDF_016: derived `ty` must implement
//! `PartialEq`.
//!
//! The per-visage `assert_derived_parity` inherent method (and its
//! `DerivedParity` trait-impl sibling) emit per-field `!=` checks
//! that require each derived field's `ty` to satisfy `PartialEq`.
//! The macro emits a `where <Ty>: PartialEq` bound on the impl block
//! so rustc's E0277 diagnostic anchors there rather than at the
//! inner `!=` token — making the error precise about the
//! responsibility (the type needs `PartialEq`, not "this method has
//! a confusing trait-resolution issue").
//!
//! This fixture pins the diagnostic on a derived `ty` lacking
//! `PartialEq`. When E_DJG_VDF_016 is restated or routed through a
//! different rule (e.g., a custom-derive in a future phase), the
//! `.stderr` snapshot here must update in lockstep so the spec and
//! the diagnostic stay aligned.

use djogi::prelude::*;

// `Site` does NOT derive `PartialEq` — only `Debug + Clone`. The
// derived field's `ty = Site` therefore violates the equality bound
// the macro emits on the parity helper's impl block.
#[derive(Debug, Clone)]
pub struct Site {
    pub name: String,
}

#[model(table = "phase85_derived_partial_eq_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = facility_site,
    ty     = Site,
    scopes = [public],
    sql    = "''",
    rust   = "Site { name: model.inbound_site.clone() }",
)]
pub struct Consignment {
    #[field(expose(public))]
    pub inbound_site: String,
}

fn main() {}
