//! Per-audience JSONB parity helper catches synthetic extra-drift (#312).
//! In-memory only (no DB): builds a leaky ProfilePublic whose `metadata`
//! carries an unknown key in `extra` (via Deserialize, since `extra` has
//! no public mutator) and asserts assert_derived_parity surfaces Drift.
//! docs/spec/jsonb-per-audience-schema.md §Compile-pass fixtures (004).

use djogi::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub stripe_customer_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
}

#[model(table = "jpa_004_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

fn main() {
    let profile = Profile {
        metadata: Jsonb::new(ProfileMetaAdmin {
            display_name: "Ann".to_string(),
            stripe_customer_id: "cus_x".to_string(),
        }),
        ..Default::default()
    };

    let in_memory: ProfilePublic = (&profile).into();

    // Leaky synthetic: same display_name but a populated `extra` map.
    let leaky_meta: Jsonb<ProfileMetaPublic> = serde_json::from_value(
        serde_json::json!({ "display_name": "Ann", "leaked": "secret" }),
    )
    .expect("deserialize leaky projection");
    assert!(!leaky_meta.extra().is_empty(), "guard: extra populated");

    let mut leaky = in_memory.clone();
    leaky.metadata = leaky_meta;

    assert!(
        matches!(
            in_memory.assert_derived_parity(&leaky),
            Err(djogi::testing::DerivedParityError::Drift { .. })
        ),
        "parity helper must catch the extra-map drift"
    );
}
