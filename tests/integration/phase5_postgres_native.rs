use djogi::DjogiEnum;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

#[model(table = "accounts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Account {
    pub name: Tracked<String>,
    pub balance: i64,
    pub note: Tracked<Option<String>>,
    #[field(version)]
    pub revision: i32,
}

#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "vehicle_status", rename_all = "snake_case")]
pub enum VehicleStatus {
    Active,
    InMaintenance,
    #[djogi_enum_variant(name = "decommissioned")]
    Retired,
}

#[model(table = "vehicles", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub status: VehicleStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostSpec {
    pub engine_cylinders: i32,
    pub brand: String,
}

#[model(table = "posts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub tags: Vec<String>,
    pub view_counts: Vec<i32>,
    pub specs: Option<Jsonb<serde_json::Value>>,
    pub published: bool,
}

#[model(table = "typed_posts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct TypedPost {
    pub title: String,
    pub specs: Option<Jsonb<PostSpec>>,
}

#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct EngineDeepSpecs {
    pub cylinders: i32,
    pub turbo: bool,
}

#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct VehicleDeepSpecs {
    pub engine: EngineDeepSpecs,
    pub weight_kg: f32,
    pub brand: String,
}

#[model(table = "vehicles_deep", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct VehicleDeep {
    pub name: String,
    pub specs: Option<Jsonb<VehicleDeepSpecs>>,
}

#[model(table = "tenant_post", pk = HeerId, tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TenantPost {
    pub org_id: String,
    pub title: String,
}

fn make_vehicle(status: VehicleStatus) -> Vehicle {
    Vehicle {
        id: <HeerId as djogi::PrimaryKey>::sentinel(),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        status,
    }
}

fn make_post(title: &str, tags: Vec<String>, view_counts: Vec<i32>) -> Post {
    Post {
        title: title.to_string(),
        tags,
        view_counts,
        specs: None,
        published: false,
        ..Default::default()
    }
}

fn make_post_with_specs(title: &str, specs: serde_json::Value) -> Post {
    Post {
        title: title.to_string(),
        tags: vec![],
        view_counts: vec![],
        specs: Some(Jsonb::new(specs)),
        published: false,
        ..Default::default()
    }
}

fn typed_specs_with_extra() -> Jsonb<PostSpec> {
    serde_json::from_str(
        r#"{"engine_cylinders":4,"brand":"TestBrand","experimental":true,"legacy_field":99}"#,
    )
    .expect("deserialize typed specs with extra keys")
}

async fn make_vehicle_deep(
    ctx: &mut djogi::DjogiContext,
    name: &str,
    cylinders: i32,
    turbo: bool,
    weight_kg: f32,
    brand: &str,
) -> VehicleDeep {
    VehicleDeep::create(
        ctx,
        VehicleDeep {
            name: name.to_string(),
            specs: Some(Jsonb::new(VehicleDeepSpecs {
                engine: EngineDeepSpecs { cylinders, turbo },
                weight_kg,
                brand: brand.to_string(),
            })),
            ..Default::default()
        },
    )
    .await
    .expect("create VehicleDeep")
}

#[djogi::djogi_test(sync_models = [Account])]
async fn tracked_round_trips_through_pg(mut ctx: djogi::DjogiContext) {
    let created = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            note: Tracked::new(None),
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    assert!(!created.name.is_dirty());
    assert_eq!(&*created.name, "alice");
    assert_eq!(*created.note, None);

    let reloaded = Account::get(&mut ctx, created.id)
        .await
        .expect("get account");
    assert!(!reloaded.name.is_dirty());
    assert!(!reloaded.note.is_dirty());
    assert_eq!(&*reloaded.name, "alice");
}

#[djogi::djogi_test(sync_models = [Account])]
async fn tracked_deref_mut_marks_dirty_and_save_cleans(mut ctx: djogi::DjogiContext) {
    let mut account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    *account.name = "bob".to_string();
    assert!(account.name.is_dirty());

    account.save(&mut ctx).await.expect("save account");
    assert!(!account.name.is_dirty());
    assert_eq!(&*account.name, "bob");
}

#[djogi::djogi_test(sync_models = [Account])]
async fn optimistic_lock_success_and_conflict(mut ctx: djogi::DjogiContext) {
    let account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("carol".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    assert_eq!(account.revision, 0);

    let mut winner = account.clone();
    let mut stale = account;
    winner.save(&mut ctx).await.expect("winner save");
    assert_eq!(winner.revision, 1);

    let result = stale.save(&mut ctx).await;
    assert!(matches!(result, Err(djogi::DjogiError::LockConflict(_))));
}

#[djogi::djogi_test(sync_models = [Vehicle])]
async fn vehicle_status_all_variants_round_trip(mut ctx: djogi::DjogiContext) {
    for status in [
        VehicleStatus::Active,
        VehicleStatus::InMaintenance,
        VehicleStatus::Retired,
    ] {
        let created = Vehicle::create(&mut ctx, make_vehicle(status))
            .await
            .expect("create vehicle");
        let reloaded = Vehicle::get(&mut ctx, created.id)
            .await
            .expect("get vehicle");
        assert_eq!(reloaded.status, status);
    }
}

#[test]
fn vehicle_status_enum_descriptor_registered() {
    let desc = inventory::iter::<djogi::descriptor::EnumDescriptor>()
        .find(|d| d.postgres_type == "vehicle_status")
        .expect("EnumDescriptor for vehicle_status must be in inventory");

    assert_eq!(desc.type_name, "VehicleStatus");
    assert_eq!(
        desc.variants,
        &["active", "in_maintenance", "decommissioned"]
    );
}

#[djogi::djogi_test(sync_models = [Post])]
async fn array_contains_overlap_and_len_filters(mut ctx: djogi::DjogiContext) {
    let rust = Post::create(
        &mut ctx,
        make_post(
            "Rust and Postgres",
            vec!["rust".into(), "postgres".into()],
            vec![1, 2, 3, 4],
        ),
    )
    .await
    .expect("create rust post");
    let python = Post::create(
        &mut ctx,
        make_post("Python only", vec!["python".into()], vec![1]),
    )
    .await
    .expect("create python post");

    // PR3: array `contains` / `overlap` (`@>` / `&&`) are PostgreSQL-
    // specific; route through `explicit_pg_predicate()` from the
    // post-flip `DjogiField` surface.
    let contains = Post::objects()
        .filter(|f| {
            f.tags()
                .explicit_pg_predicate()
                .contains(&["rust".to_string(), "postgres".to_string()])
        })
        .fetch_all(&mut ctx)
        .await
        .expect("array contains");
    assert!(contains.iter().any(|post| post.id == rust.id));
    assert!(!contains.iter().any(|post| post.id == python.id));

    let overlap = Post::objects()
        .filter(|f| {
            f.tags()
                .explicit_pg_predicate()
                .overlap(&["rust".to_string(), "java".to_string()])
        })
        .fetch_all(&mut ctx)
        .await
        .expect("array overlap");
    assert!(overlap.iter().any(|post| post.id == rust.id));
    assert!(!overlap.iter().any(|post| post.id == python.id));

    let long_arrays = Post::objects()
        .filter_expr(|f| f.view_counts().len().gt(Expr::literal(3i32)))
        .fetch_all(&mut ctx)
        .await
        .expect("array length filter");
    assert_eq!(
        long_arrays.iter().map(|post| post.id).collect::<Vec<_>>(),
        vec![rust.id]
    );
}

#[djogi::djogi_test(sync_models = [Post])]
async fn jsonb_flat_path_filter_works(mut ctx: djogi::DjogiContext) {
    let v8 = Post::create(
        &mut ctx,
        make_post_with_specs(
            "V8 Beast",
            serde_json::json!({"engine_cylinders": 8, "brand": "V8Power"}),
        ),
    )
    .await
    .expect("create v8 post");
    let eco = Post::create(
        &mut ctx,
        make_post_with_specs(
            "Eco Car",
            serde_json::json!({"engine_cylinders": 2, "brand": "EcoMobile"}),
        ),
    )
    .await
    .expect("create eco post");

    // PR3: JSONB `path()` is PostgreSQL-specific and SQL-only in 8eta;
    // route through `explicit_pg_predicate()` from the post-flip
    // `DjogiField` surface.
    let found = Post::objects()
        .filter(|f| {
            f.specs()
                .explicit_pg_predicate()
                .path::<i32>("engine_cylinders")
                .gt(4)
        })
        .fetch_all(&mut ctx)
        .await
        .expect("jsonb path filter");

    assert!(found.iter().any(|post| post.id == v8.id));
    assert!(!found.iter().any(|post| post.id == eco.id));
}

// djogi#161 — `primary_key!`-emitted custom PK newtypes (and other
// inner-type-delegating wrappers) must emit the same typed Postgres
// cast their inner SQL value type emits when used as the value generic
// of `JsonbPathRef<M, V>`. Pre-fix, wrappers inherited the default
// `IntoFilterValue::jsonb_sql_cast` body, which walks `type_name::<Self>()`
// through the built-in cast table. The wrapper's own `type_name` is
// never in the table, so JSONB path comparisons against a wrapper-typed
// payload silently emitted no cast and Postgres compared the text-
// extracted LHS lexicographically.
//
// Values `9` and `10` are the canonical numeric-vs-text divergence:
// text ordering puts `'10' < '9'` because `'1' < '9'` byte-wise,
// while numeric ordering puts `10 > 9`. A `.gt(JsonbRankId(9))` query
// must include only `10` (numeric path) and never `10` ordered before
// `9` (text path).
djogi::primary_key! {
    pub struct JsonbRankId(i64);
    sql_type = "BIGINT";
    default_sql = "0";
    bulk_sql = "SELECT 0::bigint AS id FROM generate_series(1, $1)";
}

#[djogi::djogi_test(sync_models = [Post])]
async fn jsonb_path_custom_pk_uses_numeric_cast(mut ctx: djogi::DjogiContext) {
    let nine = Post::create(
        &mut ctx,
        make_post_with_specs("Rank 9", serde_json::json!({"rank": 9})),
    )
    .await
    .expect("create rank 9 post");
    let ten = Post::create(
        &mut ctx,
        make_post_with_specs("Rank 10", serde_json::json!({"rank": 10})),
    )
    .await
    .expect("create rank 10 post");

    // `.path::<JsonbRankId>("rank").gt(JsonbRankId(9))` must emit
    // `(specs->>'rank')::int8 > $1` so Postgres orders 10 > 9 numerically.
    // The pre-djogi#161 bug skipped the `::int8` cast for `JsonbRankId`
    // (the wrapper's `type_name` was not in the cast table) and the
    // comparison ran as `(specs->>'rank') > '9'::text`, which excludes
    // `10` because `'10' < '9'` lexicographically.
    let above_nine = Post::objects()
        .filter(|f| {
            f.specs()
                .explicit_pg_predicate()
                .path::<JsonbRankId>("rank")
                .gt(JsonbRankId(9))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("custom-PK JSONB path filter");

    // Numeric semantics: only the rank-10 row passes; rank-9 does not.
    assert!(
        above_nine.iter().any(|post| post.id == ten.id),
        "rank 10 must be returned by `> 9` (numeric ordering)"
    );
    assert!(
        !above_nine.iter().any(|post| post.id == nine.id),
        "rank 9 must NOT be returned by `> 9` (strict gt)"
    );

    // Sanity: the opposite direction also matches the numeric path.
    let below_ten = Post::objects()
        .filter(|f| {
            f.specs()
                .explicit_pg_predicate()
                .path::<JsonbRankId>("rank")
                .lt(JsonbRankId(10))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("custom-PK JSONB path filter (lt)");
    assert!(
        below_ten.iter().any(|post| post.id == nine.id),
        "rank 9 must be returned by `< 10` (numeric ordering)"
    );
    assert!(
        !below_ten.iter().any(|post| post.id == ten.id),
        "rank 10 must NOT be returned by `< 10` (strict lt)"
    );
}

#[djogi::djogi_test(sync_models = [TypedPost])]
async fn typed_jsonb_round_trip_preserves_unknown_fields(mut ctx: djogi::DjogiContext) {
    let mut post = TypedPost::create(
        &mut ctx,
        TypedPost {
            title: "Typed JSONB Test".to_string(),
            specs: Some(typed_specs_with_extra()),
            ..Default::default()
        },
    )
    .await
    .expect("create typed post");

    let specs = post.specs.as_ref().expect("specs after create");
    assert_eq!(specs.data.engine_cylinders, 4);
    assert_eq!(specs.extra().len(), 2);
    assert!(specs.extra().contains_key("experimental"));

    post.specs.as_mut().unwrap().data.engine_cylinders = 6;
    post.save(&mut ctx).await.expect("save typed post");

    let reloaded = TypedPost::get(&mut ctx, post.id)
        .await
        .expect("reload typed post");
    let specs = reloaded.specs.as_ref().expect("specs after reload");
    assert_eq!(specs.data.engine_cylinders, 6);
    assert_eq!(specs.extra().len(), 2);
    assert!(specs.extra().contains_key("legacy_field"));
}

#[djogi::djogi_test(sync_models = [VehicleDeep])]
async fn typed_jsonb_deep_path_filters(mut ctx: djogi::DjogiContext) {
    let v8 = make_vehicle_deep(&mut ctx, "V8 Beast", 8, false, 1800.0, "BrandA").await;
    let eco = make_vehicle_deep(&mut ctx, "Eco Car", 3, false, 1200.0, "BrandB").await;

    // PR3: JSONB `typed()` / `path()` predicates are PostgreSQL-specific
    // and SQL-only in 8eta; route through `explicit_pg_predicate()` from
    // the post-flip `DjogiField` surface so the closure surface keeps the
    // database-locale typed-path filtering shape.
    let cylinder_matches = VehicleDeep::objects()
        .filter(|f| {
            f.specs()
                .explicit_pg_predicate()
                .typed()
                .engine()
                .cylinders()
                .gt(4)
        })
        .fetch_all(&mut ctx)
        .await
        .expect("typed cylinder filter");
    assert!(cylinder_matches.iter().any(|vehicle| vehicle.id == v8.id));
    assert!(!cylinder_matches.iter().any(|vehicle| vehicle.id == eco.id));

    let heavy_matches = VehicleDeep::objects()
        .filter(|f| {
            f.specs()
                .explicit_pg_predicate()
                .typed()
                .weight_kg()
                .gt(1500.0_f32)
        })
        .fetch_all(&mut ctx)
        .await
        .expect("typed weight filter");
    assert_eq!(
        heavy_matches
            .iter()
            .map(|vehicle| vehicle.id)
            .collect::<Vec<_>>(),
        vec![v8.id],
    );
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn set_tenant_tracks_applied_tenant_in_transaction(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");
    tx.set_tenant("org_a").await.expect("set tenant");
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));

    let created = TenantPost::create(
        &mut tx,
        TenantPost {
            org_id: "org_a".into(),
            title: "tenant row".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create tenant post");

    let fetched = TenantPost::objects()
        .filter(|f| f.org_id().eq("org_a".to_string()))
        .fetch_all(&mut tx)
        .await
        .expect("fetch tenant post");
    assert_eq!(
        fetched.iter().map(|post| post.id).collect::<Vec<_>>(),
        vec![created.id]
    );

    tx.commit().await.expect("commit transaction");
}
