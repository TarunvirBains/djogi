//! Phase 8.5 #231 — E_DJG_VDF_004: derived `name` violates the
//! general identifier-shape rules (length > 63 bytes here).
//!
//! `validate_name_shape` in `djogi-macros/src/model/derived.rs`
//! enforces length 1..=63, lowercase letters / underscores leading,
//! lowercase letters / digits / underscores in the body. The
//! uppercase-byte case has its own code (E_DJG_VDF_012); the
//! reserved-keyword case has its own code (E_DJG_VDF_014).
//!
//! This fixture exercises the length cap: a 75-byte identifier (well
//! over the 63-byte Postgres unquoted-identifier limit) must reject
//! with E_DJG_VDF_004. The bare-form `name = ...` is used to
//! exercise the `Expr::Path` arm of `parse_name_value`; the captured
//! ident is then routed through the shape validator's length check.

use djogi::prelude::*;

#[model(table = "phase85_e004_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = facility_site_with_a_very_long_name_that_exceeds_the_sixty_three_byte_cap,
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
