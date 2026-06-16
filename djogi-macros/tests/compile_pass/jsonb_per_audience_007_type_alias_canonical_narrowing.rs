//! Per-audience JSONB type-alias canonical narrowing (#312): derived-side alias
//! accepted when paired with real narrowing SQL (accept side of fail_009).
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures (007).

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

pub type PublicMeta = Jsonb<ProfileMetaPublic>;

#[model(table = "jpa_007_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = PublicMeta,
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    fn _assert(v: &ProfilePublic) -> &PublicMeta {
        &v.metadata
    }
}
