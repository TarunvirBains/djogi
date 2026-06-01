//! E_DJG_VDF_012: derived `name` contains an
//! uppercase ASCII byte.
//!
//! Postgres folds unquoted identifiers to lowercase server-side; a
//! camelCase derived `name` would silently rename on the wire and
//! break positional `FromPgRow` decode. `validate_name_shape` in
//! `djogi-macros/src/model/derived.rs` rejects any uppercase byte
//! at parse time with the dedicated E_DJG_VDF_012 diagnostic — kept
//! separate from the general shape rule (E_DJG_VDF_004) so the
//! diagnostic prose can call out the case-folding hazard directly.

use djogi::prelude::*;

#[model(table = "phase85_e012_consignments")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = facilitySite,
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
