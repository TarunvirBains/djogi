//! E_DJG_VDF_012 on a JSONB-shaped declaration: `name = Metadata`
//! carries an uppercase byte. docs/spec/jsonb-per-audience-schema.md
//! §Compile-fail fixtures.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AdminMeta {
    pub display_name: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PublicMeta {
    pub display_name: String,
}

#[model(table = "jpa_fail_003_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = Metadata,
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
