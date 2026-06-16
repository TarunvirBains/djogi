//! Per-audience JSONB basic narrowing (#312): storage Jsonb<AdminSchema>
//! exposed to [self_view, admin, export]; derived Jsonb<PublicSchema>
//! projected to [public] via jsonb_build_object. Asserts the generated
//! visage field types and projection constants.
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub bio: String,
    pub avatar_url: Option<String>,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
    pub bio: String,
    pub avatar_url: Option<String>,
}

#[model(table = "jpa_001_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name', 'bio', metadata->'bio', 'avatar_url', metadata->'avatar_url')",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone(), bio: model.metadata.data.bio.clone(), avatar_url: model.metadata.data.avatar_url.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    // Type-level assertions: the field types differ per visage.
    fn _assert_public(v: &ProfilePublic) -> &Jsonb<ProfileMetaPublic> { &v.metadata }
    fn _assert_admin(v: &ProfileAdmin) -> &Jsonb<ProfileMetaAdmin> { &v.metadata }

    // Projection-list assertions.
    assert!(
        <ProfilePublic as djogi::DjogiVisage>::PROJECTION_LIST.contains("jsonb_build_object"),
        "public projection must use jsonb_build_object"
    );
    assert!(
        <ProfilePublic as djogi::DjogiVisage>::PROJECTION_LIST.contains("AS metadata"),
        "public projection must alias the derived entry as metadata"
    );
    assert!(
        <ProfileAdmin as djogi::DjogiVisage>::COLUMNS.contains(&"metadata"),
        "admin visage carries the storage metadata column"
    );
}
