//! E_DJG_VDF_002 on a JSONB-shaped declaration: the storage `metadata`
//! column is exposed to `public` AND a `#[derived(name = metadata,
//! scopes = [public])]` targets the same scope with the same name.
//! Per-audience JSONB projection composes with the inherited column-name
//! collision guard. See docs/spec/jsonb-per-audience-schema.md
//! §Compile-fail fixtures.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AdminMeta {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PublicMeta {
    pub display_name: String,
}

#[model(table = "jpa_fail_001_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<PublicMeta>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(PublicMeta { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(public, admin))]
    pub metadata: Jsonb<AdminMeta>,
}

fn main() {}
