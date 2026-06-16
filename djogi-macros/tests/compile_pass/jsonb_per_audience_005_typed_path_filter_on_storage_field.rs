//! Per-audience JSONB storage typed-path filter (#312): storage Jsonb<ProfileMetaAdmin>
//! derives JsonbSchema. Assert it supports the typed path filter bridge.
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures.

use djogi::prelude::*;
use djogi::JsonbSchema;

#[derive(JsonbSchema, serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

#[model(table = "jpa_005_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    let _path_fn = |f: ProfileFields| {
        f.metadata()
            .explicit_pg_predicate()
            .typed()
            .display_name()
            .eq("Ann".to_string())
    };
}
