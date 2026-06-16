//! E_DJG_VDF_017 (new in #312): derived-side type-alias passthrough.
//! `ty = PublicMeta` (a `Jsonb<ProfileMetaPublic>` alias) with
//! `sql = "metadata"` over a directly-spelled `Jsonb` storage column is
//! rejected — the alias on the derived `ty` is not an escape hatch; the
//! guard keys on the storage column's direct `Jsonb<...>` type and the
//! simple-ident sql. The paired compile-pass fixture
//! `jsonb_per_audience_007_type_alias_canonical_narrowing.rs` confirms the
//! same alias is accepted with real narrowing SQL.
//! docs/spec/jsonb-per-audience-schema.md §Error taxonomy extension.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

pub type PublicMeta = Jsonb<ProfileMetaPublic>;

#[model(table = "jpa_fail_009_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = PublicMeta,
    scopes = [public],
    sql    = "metadata",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {}
