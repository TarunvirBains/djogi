//! E_DJG_VDF_007 on a JSONB-shaped declaration: a `;` outside any string
//! literal in `sql`. docs/spec/jsonb-per-audience-schema.md
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

#[model(table = "jpa_fail_005_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<PublicMeta>",
    scopes = [public],
    sql    = "metadata; DROP TABLE profiles",
    rust   = "Jsonb::new(PublicMeta { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(admin))]
    pub metadata: Jsonb<AdminMeta>,
}

fn main() {}
