//! E_DJG_VDF_017 (new in #312): same-name simple-column passthrough.
//! `name = metadata, sql = "metadata"` over a `Jsonb` storage column
//! ships the full admin JSON to the wire and leaks admin-only keys via
//! the projected `Jsonb<PublicMeta>::extra` on re-serialize. Rejected at
//! parse time. docs/spec/jsonb-per-audience-schema.md §Error taxonomy
//! extension.

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

#[model(table = "jpa_fail_006_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "metadata",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {}
