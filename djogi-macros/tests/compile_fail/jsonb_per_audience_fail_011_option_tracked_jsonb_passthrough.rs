//! E_DJG_VDF_017 (#312): `Option<Tracked<Jsonb<T>>>` storage passthrough.
//! A storage field declared `Option<Tracked<Jsonb<ProfileMetaAdmin>>>` with
//! `sql = "metadata"` is a simple-column passthrough: it ships the full
//! admin JSON to the wire and leaks admin-only keys via the projected
//! `Jsonb<ProfileMetaPublic>::extra` on re-serialize. Neither the nullable
//! `Option<_>` wrapper nor the dirty-tracking `Tracked<_>` wrapper changes
//! the column's JSONB shape, so the guard must strip both and reject the
//! passthrough at parse time.
//! Paired compile-pass:
//! jsonb_per_audience_008_tracked_jsonb_canonical_narrowing.rs
//! docs/spec/jsonb-per-audience-schema.md §Error taxonomy extension.
//!
//! Accessor note: `model.metadata` of type
//! `Option<Tracked<Jsonb<ProfileMetaAdmin>>>` calls `Option::as_ref()`
//! directly, yielding `Option<&Tracked<Jsonb<ProfileMetaAdmin>>>`. In the
//! `|t|` closure, `t: &Tracked<Jsonb<ProfileMetaAdmin>>` auto-derefs to
//! `Jsonb<ProfileMetaAdmin>`, so `.data.display_name.clone()` is correct.
//! `Tracked<T>` has no `.value()` method.

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

#[model(table = "jpa_fail_011_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Option<Jsonb<ProfileMetaPublic>>",
    scopes = [public],
    sql    = "metadata",
    rust   = "Some(Jsonb::new(ProfileMetaPublic { display_name: model.metadata.as_ref().map(|t| t.data.display_name.clone()).unwrap_or_default() }))",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Option<Tracked<Jsonb<ProfileMetaAdmin>>>,
}

fn main() {}
