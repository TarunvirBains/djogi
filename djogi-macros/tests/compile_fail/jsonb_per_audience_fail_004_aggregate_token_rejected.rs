//! E_DJG_VDF_009 on a JSONB-shaped declaration: `sql = "jsonb_agg(metadata)"`
//! contains a recognised aggregate token. The token scan is context-blind
//! and fires unconditionally until Shape V `aggregate = true` lands
//! (deferred — djogi#226-container). docs/spec/jsonb-per-audience-schema.md
//! §Aggregate token discipline.

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AdminMeta {
    pub display_name: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PublicMeta {
    pub display_name: String,
}

#[model(table = "jpa_fail_004_profiles")]
#[derive(Model, Debug, Clone)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<PublicMeta>",
    scopes = [public],
    sql    = "jsonb_agg(metadata)",
    rust   = "Jsonb::new(PublicMeta { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(admin))]
    pub metadata: Jsonb<AdminMeta>,
}

fn main() {}
