//! Per-audience JSONB two narrow audiences (#312): storage Jsonb<ProfileMetaAdmin>
//! exposed to [self_view, admin]; derived Jsonb<ProfileMetaPublic>
//! projected to [public] and Jsonb<ProfileMetaExport> projected to [export]
//! via jsonb_build_object. Asserts the generated visage field types.
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
    pub export_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaExport {
    pub display_name: String,
    pub export_id: String,
}

#[model(table = "jpa_002_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaExport>",
    scopes = [export],
    sql    = "jsonb_build_object('display_name', metadata->'display_name', 'export_id', metadata->'export_id')",
    rust   = "Jsonb::new(ProfileMetaExport { display_name: model.metadata.data.display_name.clone(), export_id: model.metadata.data.export_id.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    // Type-level assertions: the field types differ per visage.
    fn _assert_public(v: &ProfilePublic) -> &Jsonb<ProfileMetaPublic> { &v.metadata }
    fn _assert_export(v: &ProfileExport) -> &Jsonb<ProfileMetaExport> { &v.metadata }
    fn _assert_admin(v: &ProfileAdmin) -> &Jsonb<ProfileMetaAdmin> { &v.metadata }
}
