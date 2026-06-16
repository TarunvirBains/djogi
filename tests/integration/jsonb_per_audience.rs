//! Issue #312 — per-audience JSONB schema projection: live integration
//! coverage. The model carries the full admin schema; ProfilePublic
//! projects a narrow shape via #[derived(... jsonb_build_object ...)].
//!
//! Every test uses #[djogi_test(sync_models = [...])] and the typed surface
//! (Model::create, VisageQuerySet, assert_derived_parity); no raw_*.

use djogi::prelude::*;
use djogi::testing::DerivedParityError;
use djogi::JsonbSchema;

#[derive(JsonbSchema, serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub bio: String,
    pub avatar_url: Option<String>,
    pub stripe_customer_id: String,
    pub analytics_id: String,
    pub last_referrer: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaPublic {
    pub display_name: String,
    pub bio: String,
    pub avatar_url: Option<String>,
}

#[model(table = "jpa_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
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

fn admin_meta() -> ProfileMetaAdmin {
    ProfileMetaAdmin {
        display_name: "Ann".to_string(),
        bio: "hi".to_string(),
        avatar_url: Some("a.png".to_string()),
        stripe_customer_id: "cus_secret".to_string(),
        analytics_id: "an_secret".to_string(),
        last_referrer: Some("ref_secret".to_string()),
    }
}

#[djogi::djogi_test(sync_models = [Profile])]
async fn profile_public_omits_admin_only_keys(mut ctx: DjogiContext) {
    let profile = Profile::create(
        &mut ctx,
        Profile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let public = ProfilePublic::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch ProfilePublic");

    assert_eq!(public.metadata.data.display_name, "Ann");
    assert!(
        public.metadata.extra().is_empty(),
        "narrow projection must have empty extra; got {:?}",
        public.metadata.extra()
    );

    let json = serde_json::to_string(&public).expect("serialize ProfilePublic");
    assert!(!json.contains("stripe_customer_id"), "leak: {json}");
    assert!(!json.contains("analytics_id"), "leak: {json}");
    assert!(!json.contains("last_referrer"), "leak: {json}");
}

#[djogi::djogi_test(sync_models = [Profile])]
async fn profile_admin_carries_full_schema(mut ctx: DjogiContext) {
    let profile = Profile::create(
        &mut ctx,
        Profile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let admin = ProfileAdmin::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch ProfileAdmin");

    assert_eq!(admin.metadata.data.display_name, "Ann");
    assert_eq!(admin.metadata.data.stripe_customer_id, "cus_secret");
    assert_eq!(admin.metadata.data.analytics_id, "an_secret");
}

#[djogi::djogi_test(sync_models = [Profile])]
async fn parity_helper_passes_for_correct_projection(mut ctx: DjogiContext) {
    let profile = Profile::create(
        &mut ctx,
        Profile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let in_memory: ProfilePublic = (&profile).into();
    let from_db = ProfilePublic::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch ProfilePublic");

    in_memory
        .assert_derived_parity(&from_db)
        .expect("correct projection must pass parity");
}

#[djogi::djogi_test(sync_models = [Profile])]
async fn parity_helper_catches_storage_drift(mut ctx: DjogiContext) {
    let profile = Profile::create(
        &mut ctx,
        Profile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let in_memory: ProfilePublic = (&profile).into();

    let leaky_meta: Jsonb<ProfileMetaPublic> = serde_json::from_value(serde_json::json!({
        "display_name": "Ann", "bio": "hi", "avatar_url": "a.png", "leaked": "secret"
    }))
    .expect("deserialize leaky projection");
    let mut leaky = in_memory.clone();
    leaky.metadata = leaky_meta;

    match in_memory.assert_derived_parity(&leaky) {
        Err(DerivedParityError::Drift { field, .. }) => assert_eq!(field, "metadata"),
        other => panic!("expected Drift on metadata, got {other:?}"),
    }
}

#[djogi::djogi_test(sync_models = [Profile])]
async fn storage_field_still_supports_typed_path_filter(mut ctx: DjogiContext) {
    Profile::create(
        &mut ctx,
        Profile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let found = Profile::objects()
        .filter(|f| f.metadata().explicit_pg_predicate().typed().display_name().eq("Ann".to_string()))
        .fetch_all(&mut ctx)
        .await
        .expect("typed-path filter on storage field");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].metadata.data.display_name, "Ann");
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ThemeAdmin {
    pub color: String,
    pub internal_palette_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ThemePublic {
    pub color: String,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct NestedAdmin {
    pub theme: Jsonb<ThemeAdmin>,
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct NestedPublic {
    pub theme: Jsonb<ThemePublic>,
}

#[model(table = "jpa_nested_recursive_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<NestedPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('theme', jsonb_build_object('color', metadata->'theme'->'color'))",
    rust   = "Jsonb::new(NestedPublic { theme: Jsonb::new(ThemePublic { color: model.metadata.data.theme.data.color.clone() }) })",
)]
pub struct NestedRecursiveProfile {
    #[field(expose(admin))]
    pub metadata: Jsonb<NestedAdmin>,
}

#[model(table = "jpa_nested_shallow_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<NestedPublic>",
    scopes = [public],
    sql    = "jsonb_build_object('theme', metadata->'theme')",
    rust   = "Jsonb::new(NestedPublic { theme: Jsonb::new(ThemePublic { color: model.metadata.data.theme.data.color.clone() }) })",
)]
pub struct NestedShallowProfile {
    #[field(expose(admin))]
    pub metadata: Jsonb<NestedAdmin>,
}

fn nested_admin() -> NestedAdmin {
    NestedAdmin {
        theme: Jsonb::new(ThemeAdmin {
            color: "red".to_string(),
            internal_palette_id: "pal_secret".to_string(),
        }),
    }
}

#[djogi::djogi_test(sync_models = [NestedRecursiveProfile])]
async fn profile_public_recursive_nested_jsonb_omits_admin_only_keys(mut ctx: DjogiContext) {
    let profile = NestedRecursiveProfile::create(
        &mut ctx,
        NestedRecursiveProfile {
            metadata: Jsonb::new(nested_admin()),
            ..Default::default()
        },
    )
    .await
    .expect("create nested recursive profile");

    let public = NestedRecursiveProfilePublic::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch public");

    assert_eq!(public.metadata.data.theme.data.color, "red");
    assert!(
        public.metadata.data.theme.extra().is_empty(),
        "inner extra must be empty"
    );
    assert!(
        public.metadata.extra().is_empty(),
        "outer extra must be empty"
    );

    let json = serde_json::to_string(&public).expect("serialize");
    assert!(!json.contains("internal_palette_id"), "nested leak: {json}");
}

#[djogi::djogi_test(sync_models = [NestedShallowProfile])]
async fn profile_public_non_recursive_nested_projection_leaks_caught_by_parity(
    mut ctx: DjogiContext,
) {
    let profile = NestedShallowProfile::create(
        &mut ctx,
        NestedShallowProfile {
            metadata: Jsonb::new(nested_admin()),
            ..Default::default()
        },
    )
    .await
    .expect("create nested shallow profile");

    let in_memory: NestedShallowProfilePublic = (&profile).into();
    let from_db = NestedShallowProfilePublic::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch public");

    match in_memory.assert_derived_parity(&from_db) {
        Err(DerivedParityError::Drift { field, .. }) => assert_eq!(field, "metadata"),
        other => panic!("expected Drift from shallow nested leak, got {other:?}"),
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default, PartialEq, Debug, Clone)]
pub struct ProfileMetaRenamed {
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[model(table = "jpa_wire_key_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaRenamed>",
    scopes = [public],
    sql    = "jsonb_build_object('display_name', metadata->'display_name')",
    rust   = "Jsonb::new(ProfileMetaRenamed { display_name: model.metadata.data.display_name.clone() })",
)]
pub struct WireKeyProfile {
    #[field(expose(admin))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

#[djogi::djogi_test(sync_models = [WireKeyProfile])]
async fn profile_public_wire_key_mismatch_required_field_fails_decode(mut ctx: DjogiContext) {
    WireKeyProfile::create(
        &mut ctx,
        WireKeyProfile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let err = WireKeyProfilePublic::limit(1)
        .fetch_one(&mut ctx)
        .await
        .expect_err("required renamed field must fail decode on wire-key mismatch");

    match err {
        DjogiError::Visage(VisageError::DbComputedTypeMismatch {
            visage,
            field,
            expected,
            actual: _,
        }) => {
            assert_eq!(visage, "WireKeyProfilePublic");
            assert_eq!(field, "metadata");
            assert!(
                expected.contains("Jsonb"),
                "expected names the narrow type: {expected}"
            );
        }
        other => panic!("expected DbComputedTypeMismatch, got {other:?}"),
    }
}

#[model(table = "jpa_compound_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "coalesce(metadata, '{}'::jsonb)",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone(), bio: model.metadata.data.bio.clone(), avatar_url: model.metadata.data.avatar_url.clone() })",
)]
pub struct CompoundProfile {
    #[field(expose(admin))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}

#[djogi::djogi_test(sync_models = [CompoundProfile])]
async fn profile_public_compound_coalesce_passthrough_caught_by_parity(mut ctx: DjogiContext) {
    let profile = CompoundProfile::create(
        &mut ctx,
        CompoundProfile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let in_memory: CompoundProfilePublic = (&profile).into();
    let from_db = CompoundProfilePublic::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect("fetch public");

    match in_memory.assert_derived_parity(&from_db) {
        Err(DerivedParityError::Drift { field, .. }) => assert_eq!(field, "metadata"),
        other => panic!("expected Drift from compound passthrough, got {other:?}"),
    }
}

pub type AliasedAdminMeta = Jsonb<ProfileMetaAdmin>;

#[model(table = "jpa_storage_alias_profiles")]
#[derive(Model, Debug, Clone, PartialEq)]
#[derived(
    name   = metadata,
    ty     = "Jsonb<ProfileMetaPublic>",
    scopes = [public],
    sql    = "metadata",
    rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone(), bio: model.metadata.data.bio.clone(), avatar_url: model.metadata.data.avatar_url.clone() })",
)]
pub struct StorageAliasProfile {
    #[field(expose(admin))]
    pub metadata: AliasedAdminMeta,
}

#[djogi::djogi_test(sync_models = [StorageAliasProfile])]
async fn profile_public_storage_side_alias_passthrough_caught_by_parity(mut ctx: DjogiContext) {
    let profile = StorageAliasProfile::create(
        &mut ctx,
        StorageAliasProfile {
            metadata: Jsonb::new(admin_meta()),
            ..Default::default()
        },
    )
    .await
    .expect("create profile");

    let in_memory: StorageAliasProfilePublic = (&profile).into();
    let from_db = StorageAliasProfilePublic::filter(|f| f.id().eq(profile.id))
        .fetch_one(&mut ctx)
        .await
        .expect
        ("fetch public");

    match in_memory.assert_derived_parity(&from_db) {
        Err(DerivedParityError::Drift { field, .. }) => assert_eq!(field, "metadata"),
        other => panic!("expected Drift from storage-side alias passthrough, got {other:?}"),
    }
}
