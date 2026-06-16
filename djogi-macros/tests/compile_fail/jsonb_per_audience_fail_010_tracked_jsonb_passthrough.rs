//! E_DJG_VDF_017 (#312): `Tracked<Jsonb<T>>` storage passthrough.
//! A storage field declared `Tracked<Jsonb<ProfileMetaAdmin>>` with
//! `sql = "metadata"` is a simple-column passthrough: it ships the full
//! admin JSON to the wire and leaks admin-only keys via the projected
//! `Jsonb<ProfileMetaPublic>::extra` on re-serialize. The dirty-tracking
//! `Tracked<_>` wrapper does not change the column's JSONB shape, so the
//! guard must strip it and reject the passthrough at parse time.
//! Paired compile-pass:
//! jsonb_per_audience_008_tracked_jsonb_canonical_narrowing.rs
//! docs/spec/jsonb-per-audience-schema.md §Error taxonomy extension.
//!
//! Accessor note: `Tracked<T>` exposes its inner value only via
//! `Deref<Target=T>` — there is no `.value()` method. `model.metadata` of
//! type `Tracked<Jsonb<ProfileMetaAdmin>>` auto-derefs to
//! `Jsonb<ProfileMetaAdmin>`, so `.data.display_name.clone()` is the
//! correct accessor chain.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

#[model(table = "jpa_fail_010_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "metadata",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Tracked<Jsonb<ProfileMetaAdmin>>,
}

fn main() {}
