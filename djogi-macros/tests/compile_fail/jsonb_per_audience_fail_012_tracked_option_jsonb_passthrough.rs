//! E_DJG_VDF_017 (#312): `Tracked<Option<Jsonb<T>>>` storage passthrough.
//! A storage field declared `Tracked<Option<Jsonb<ProfileMetaAdmin>>>` with
//! `sql = "metadata"` is a simple-column passthrough: it ships the full
//! admin JSON to the wire and leaks admin-only keys via the projected
//! `Jsonb<ProfileMetaPublic>::extra` on re-serialize. Neither the
//! dirty-tracking `Tracked<_>` wrapper nor the nullable `Option<_>` wrapper
//! changes the column's JSONB shape, so the guard must strip both — in
//! either nesting order — and reject the passthrough at parse time.
//! Paired compile-pass:
//! jsonb_per_audience_008_tracked_jsonb_canonical_narrowing.rs
//! docs/spec/jsonb-per-audience-schema.md §Error taxonomy extension.
//!
//! Accessor note: `model.metadata` of type
//! `Tracked<Option<Jsonb<ProfileMetaAdmin>>>` auto-derefs via
//! `Deref<Target=Option<Jsonb<ProfileMetaAdmin>>>`, so `.as_ref()` is called
//! directly on the auto-dereffed `Option` (no explicit `*` needed),
//! yielding `Option<&Jsonb<ProfileMetaAdmin>>`. In the `|j|` closure,
//! `j: &Jsonb<ProfileMetaAdmin>`, so `.data.display_name.clone()` is
//! correct. The `.map(...)` returns `Option<Jsonb<ProfileMetaPublic>>`
//! directly, matching `ty = "Option<Jsonb<ProfileMetaPublic>>"`.
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

#[model(table = "jpa_fail_012_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Option<Jsonb<ProfileMetaPublic>>",
    scopes = [public],
    sql    = "metadata",
    rust   = "model.metadata.as_ref().map(|j| Jsonb::new(ProfileMetaPublic { display_name: j.data.display_name.clone() }))",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Tracked<Option<Jsonb<ProfileMetaAdmin>>>,
}

fn main() {}
