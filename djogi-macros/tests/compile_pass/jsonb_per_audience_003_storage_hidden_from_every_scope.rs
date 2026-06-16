//! Per-audience JSONB storage hidden from every scope (#312): storage Jsonb<ProfileMetaAdmin>
//! exposed to [none]; derived Jsonb<ProfileMetaPublic> / Jsonb<ProfileMetaSelf> / Jsonb<ProfileMetaAdmin>
//! projected to [public] / [self_view] / [admin] respectively via jsonb_build_object.
//! Asserts the generated visage field types.
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaSelf {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

#[model(table = "jpa_003_profiles")]
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
    ty     = "Jsonb<ProfileMetaSelf>",
    scopes = [self_view],
    sql    = "jsonb_build_object('display_name', metadata->'display_name', 'stripe_customer_id', metadata->'stripe_customer_id')",
    rust   = "Jsonb::new(ProfileMetaSelf { display_name: model.metadata.data.display_name.clone(), stripe_customer_id: model.metadata.data.stripe_customer_id.clone() })",
)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaAdmin>",
    scopes = [admin],
    sql    = "jsonb_build_object('display_name', metadata->'display_name', 'stripe_customer_id', metadata->'stripe_customer_id')",
    rust   = "Jsonb::new(ProfileMetaAdmin { display_name: model.metadata.data.display_name.clone(), stripe_customer_id: model.metadata.data.stripe_customer_id.clone() })",
)]
pub struct Profile {
    #[field(expose(none))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    // Type-level assertions: the field types differ per visage.
    fn _assert_public(v: &ProfilePublic) -> &Jsonb<ProfileMetaPublic> { &v.metadata }
    fn _assert_self(v: &ProfileSelfView) -> &Jsonb<ProfileMetaSelf> { &v.metadata }
    fn _assert_admin(v: &ProfileAdmin) -> &Jsonb<ProfileMetaAdmin> { &v.metadata }
}
