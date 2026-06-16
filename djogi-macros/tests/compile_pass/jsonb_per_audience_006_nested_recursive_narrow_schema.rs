//! Per-audience JSONB nested recursive narrowing (#312): storage nested Jsonb
//! narrowed recursively via jsonb_build_object. Asserts it compiles.
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures (006).

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ThemeAdmin {
    pub color: String,
    pub internal_palette_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ThemePublic {
    pub color: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub theme: Jsonb<ThemeAdmin>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub theme: Jsonb<ThemePublic>,
}

#[model(table = "jpa_006_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('theme', jsonb_build_object('color', metadata->'theme'->'color'))",
    rust   = "Jsonb::new(ProfileMetaPublic { theme: Jsonb::new(ThemePublic { color: model.metadata.data.theme.data.color.clone() }) })",
)]
pub struct Profile {
    #[field(expose(admin))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    fn _assert(v: &ProfilePublic) -> &Jsonb<ProfileMetaPublic> {
        &v.metadata
    }
}
