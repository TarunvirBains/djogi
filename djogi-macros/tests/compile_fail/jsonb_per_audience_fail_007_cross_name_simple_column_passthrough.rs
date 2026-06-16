//! E_DJG_VDF_017 (new in #312): cross-name simple-column passthrough.
//! The derived `name` differs from the source column ident, but the guard
//! still fires — the visage field alias does not change the projected
//! `Jsonb<PublicMeta>` decode/serialize leak path.
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

#[model(table = "jpa_fail_007_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata_public_view,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [admin],
    sql    = "metadata",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {}
