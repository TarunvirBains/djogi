//! Per-audience JSONB canonical narrowing with `Tracked<Jsonb<T>>` storage
//! (#312): storage field declared `Tracked<Jsonb<ProfileMetaAdmin>>`; derived
//! `Jsonb<ProfileMetaPublic>` projected to [public] via `jsonb_build_object`.
//! Confirms the E_DJG_VDF_017 guard does NOT fire on canonical narrowing when
//! the storage column is wrapped in the `#[field(track)]` dirty-tracking
//! wrapper — the `sql` is an explicit narrowing builder, not a simple-column
//! passthrough.
//! Paired compile-fail:
//! jsonb_per_audience_fail_010_tracked_jsonb_passthrough.rs
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures.
//!
//! Accessor note: `model.metadata` of type `Tracked<Jsonb<ProfileMetaAdmin>>`
//! auto-derefs via `Deref<Target=Jsonb<ProfileMetaAdmin>>`, so
//! `.data.display_name.clone()` and `.data.bio.clone()` are correct.
//! `Tracked<T>` has no `.value()` method.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub bio: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
    pub bio: String,
}

#[model(table = "jpa_008_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name', 'bio', metadata->'bio')",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone(), bio: model.metadata.data.bio.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Tracked<Jsonb<ProfileMetaAdmin>>,
}

fn main() {}
