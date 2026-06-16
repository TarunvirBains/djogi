//! E_DJG_VDF_017 (new in #312): quoted simple-column passthrough.
//! `sql = "\"metadata\""` normalises to the same storage column ident as
//! the bare form, so the guard rejects it identically. Quoting an
//! identifier is not a narrowing expression.
//! docs/spec/jsonb-per-audience-schema.md §Error taxonomy extension.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

#[model(table = "jpa_fail_008_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "\"metadata\"",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {}
