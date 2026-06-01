//! E_DJG_VDF_016: derived `ty` must implement
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
//!
//! # Why this fixture supplies every other bound `Site` needs
//!
//! The visage struct emission attaches `Debug, Clone,
//! serde::Serialize, serde::Deserialize` to every visage; the
//! `FromPgRow` impl additionally demands `Site: FromSql` via
//! `decode_derived_at::<Site>(...)`. Without those bounds in place
//! the diagnostic would cascade with collateral
//! `Site: Serialize` / `Site: Deserialize` / `Site: FromSql` errors
//! that have nothing to do with the parity-helper contract this
//! fixture pins. `Site` therefore derives serde and hand-rolls a
//! pass-through `FromSql` impl (forwarding to `String` since the
//! type is a single-field wrapper), narrowing the `.stderr`
//! snapshot to the intended `Site: PartialEq` diagnostic.

use djogi::__private::postgres_types::{FromSql, Type};
use djogi::prelude::*;
use std::error::Error;

// `Site` does NOT derive `PartialEq` — only `Debug + Clone +
// Serialize + Deserialize` plus a manual `FromSql` pass-through.
// The derived field's `ty = Site` therefore violates ONLY the
// equality bound the macro emits on the parity helper's impl block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Site {
    pub name: String,
}

// Pass-through `FromSql` impl so the visage's `FromPgRow` decoder
// path (which routes derived columns through `decode_derived_at::<Site>`)
// resolves the trait bound without surfacing as a collateral error
// in the `.stderr` snapshot.
impl<'a> FromSql<'a> for Site {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let name = <String as FromSql>::from_sql(ty, raw)?;
        Ok(Site { name })
    }

    fn accepts(ty: &Type) -> bool {
        <String as FromSql>::accepts(ty)
    }
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
