//! E_DJG_VDF_003 on a JSONB-shaped declaration: two `#[derived(name =
//! metadata, scopes = [public])]` entries collide in the same scope.
//! docs/spec/jsonb-per-audience-schema.md §Compile-fail fixtures.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AdminMeta {
    pub display_name: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PublicMeta {
    pub display_name: String,
}

#[model(table = "jpa_fail_002_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<PublicMeta>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(PublicMeta { display_name: model.metadata.data.display_name.clone() })",
)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<PublicMeta>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(PublicMeta { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(admin))]
    pub metadata: Jsonb<AdminMeta>,
}

fn main() {}
