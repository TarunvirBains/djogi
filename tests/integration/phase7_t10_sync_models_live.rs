//! Phase 7 T10 — live-PG integration tests for
//! `#[djogi_test(sync_models = [...])]` (closes #18).
//!
//! Each test provisions a fresh `djogi_test_<uuid>` database via the
//! Phase 5-Zero harness (`#[djogi_test]`), opts into `sync_models`
//! to auto-create the listed tables through the Phase 7 migration
//! engine (T1 projection → T2 diff → T3 SQL emit + segment plan),
//! then exercises the resulting schema with real CRUD round-trips.
//!
//! # What these tests prove
//!
//! - Single-model `sync_models` materialises one `CREATE TABLE` plus
//!   the framework columns and runs CRUD round-trip cleanly.
//! - Multi-model `sync_models` topologically sorts FK-dependent
//!   tables (parent before child) regardless of attribute argument
//!   order.
//! - `Jsonb<T>` fields lower to a `jsonb` column.
//! - `extensions = [...]` provisioning happens BEFORE `sync_models`
//!   in the macro emission order, so spatial-field tables can
//!   reference PostGIS types.
//! - M2M through-models (composite shape with paired FKs) materialise
//!   alongside their targets.
//! - FK cycles (A → B → A) materialise via the migration engine's
//!   cycle-breaking path.
//! - Calling `sync_models` directly with an FK target NOT in the
//!   list returns a clean runtime error naming the missing model
//!   and the referencing column.
//! - User-declared `IndexSpec` entries route through the same SQL
//!   emitter the production migration engine uses.
//!
//! # No regex
//!
//! Per project rule, this file uses byte-level / `pg_catalog` lookups
//! for every assertion — no regex engine dependency anywhere.

#![allow(dead_code)] // Test models reference their descriptors; some
// fields are populated via DB defaults, never constructed in Rust.

use djogi::descriptor::{IndexKind, IndexTarget};
use djogi::prelude::*;
use djogi::relation::ForeignKey;

/// Sentinel framework-field block reused across every `no_default`
/// construction. The DB defaults overwrite each via `RETURNING *`
/// on the INSERT, so the local sentinel never escapes the wrapper.
fn sentinel_id() -> HeerId {
    <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel()
}

fn sentinel_dt() -> DateTime {
    DateTime::UNIX_EPOCH
}

// ───────────────────────────────────────────────────────────────────
// Scenario 1 — single-model `sync_models = [Widget]`
// ───────────────────────────────────────────────────────────────────

/// Standalone model with the framework columns plus a few user
/// fields and one declared composite index. `sync_models` must
/// emit `CREATE TABLE`, the BTree PK index that comes from the PK
/// shape, AND the `(name)` index declared on the model.
#[model(table = "t10_widgets_solo", pk = HeerId, indexes(
    index(fields = [name]),
))]
#[derive(Debug, Clone)]
pub struct WidgetSolo {
    pub name: String,
    pub price_cents: i32,
}

#[djogi::djogi_test(sync_models = [WidgetSolo])]
async fn single_model_sync_creates_table_and_supports_crud(mut ctx: djogi::DjogiContext) {
    // The table exists — verify via pg_catalog so the assertion
    // does not depend on Djogi's own ORM layer (which would
    // succeed silently if the table were created somewhere else).
    let exists: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_class \
             WHERE relname = 't10_widgets_solo' AND relkind = 'r'",
            &[],
        )
        .await
        .expect("pg_class lookup");
    assert_eq!(exists, 1, "sync_models must create the table");

    // Round-trip through Model::create / Model::get to prove the
    // column types match the descriptor.
    let w = WidgetSolo::create(
        &mut ctx,
        WidgetSolo {
            name: "hammer".into(),
            price_cents: 1995,
            ..Default::default()
        },
    )
    .await
    .expect("WidgetSolo::create succeeds against sync_models-created table");
    assert_eq!(w.name, "hammer");
    assert_eq!(w.price_cents, 1995);
    assert!(
        w.id.as_i64() > 0,
        "DB-generated HeerId must be positive (default function in place)"
    );

    let reloaded = WidgetSolo::get(&mut ctx, w.id)
        .await
        .expect("WidgetSolo::get round-trips the row");
    assert_eq!(reloaded.name, "hammer");

    // The user-declared `(name)` index must exist alongside the PK
    // index. We assert by counting rows on `pg_indexes` for the
    // table — at least 2 indexes (PK + name).
    let idx_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_indexes \
             WHERE tablename = 't10_widgets_solo'",
            &[],
        )
        .await
        .expect("pg_indexes lookup");
    assert!(
        idx_count >= 2,
        "expected at least 2 indexes on t10_widgets_solo (PK + name); got {idx_count}"
    );
}

// ───────────────────────────────────────────────────────────────────
// Scenario 2 — multi-model with FK; parent before child via topo-sort
// ───────────────────────────────────────────────────────────────────

#[model(table = "t10_categories", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Category {
    pub name: String,
}

/// `Widget` references `Category` via FK. The attribute order in
/// `sync_models = [...]` is Widget first, Category second —
/// purposefully reversed from the FK dependency. The migration
/// engine's topo-sort must reorder these so `t10_categories`
/// emits before `t10_widgets`.
#[model(table = "t10_widgets", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Widget {
    pub category_id: ForeignKey<Category>,
    pub name: String,
}

fn widget_for_insert(name: &str, category: &Category) -> Widget {
    Widget {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        category_id: ForeignKey::new(category.id),
        name: name.into(),
    }
}

#[djogi::djogi_test(sync_models = [Widget, Category])]
async fn multi_model_fk_dependency_topo_sorts(mut ctx: djogi::DjogiContext) {
    // Both tables exist.
    let cat_exists: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_class WHERE relname = 't10_categories'",
            &[],
        )
        .await
        .unwrap();
    let widget_exists: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_class WHERE relname = 't10_widgets'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(cat_exists, 1);
    assert_eq!(widget_exists, 1);

    // FK constraint exists from t10_widgets.category_id to t10_categories(id).
    // The constraint's existence proves the topo-sort emitted parent
    // before child — a reversed order would fail with "relation
    // does not exist" before this point.
    let fk_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_constraint c \
             JOIN pg_class src ON src.oid = c.conrelid \
             JOIN pg_class tgt ON tgt.oid = c.confrelid \
             WHERE c.contype = 'f' \
               AND src.relname = 't10_widgets' \
               AND tgt.relname = 't10_categories'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        fk_count, 1,
        "expected one FK from t10_widgets to t10_categories"
    );

    // CRUD round-trip across the FK.
    let cat = Category::create(
        &mut ctx,
        Category {
            name: "tools".into(),
            ..Default::default()
        },
    )
    .await
    .expect("Category::create");

    let w = Widget::create(&mut ctx, widget_for_insert("wrench", &cat))
        .await
        .expect("Widget::create against the FK target");
    assert_eq!(w.name, "wrench");
}

// ───────────────────────────────────────────────────────────────────
// Scenario 3 — `Jsonb<T>` field lowers to `jsonb`
// ───────────────────────────────────────────────────────────────────

#[derive(djogi::JsonbSchema, serde::Serialize, serde::Deserialize, Default, Debug, Clone)]
pub struct UserPrefs {
    pub theme: String,
    pub notifications_enabled: bool,
}

#[model(table = "t10_users_prefs", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct UserWithPrefs {
    pub email: String,
    pub prefs: Jsonb<UserPrefs>,
}

#[djogi::djogi_test(sync_models = [UserWithPrefs])]
async fn jsonb_field_lowers_to_jsonb_column(mut ctx: djogi::DjogiContext) {
    // Verify the column type is `jsonb` via information_schema.
    let column_type: String = ctx
        .raw_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 't10_users_prefs' AND column_name = 'prefs'",
            &[],
        )
        .await
        .expect("prefs column data_type lookup");
    assert_eq!(
        column_type, "jsonb",
        "Jsonb<T> must lower to a `jsonb` column"
    );

    // Round-trip a Jsonb value through Model::create / Model::get.
    let user = UserWithPrefs::create(
        &mut ctx,
        UserWithPrefs {
            email: "alice@example.com".into(),
            prefs: Jsonb::new(UserPrefs {
                theme: "dark".into(),
                notifications_enabled: true,
            }),
            ..Default::default()
        },
    )
    .await
    .expect("UserWithPrefs::create round-trip");
    assert_eq!(user.prefs.data.theme, "dark");
    assert!(user.prefs.data.notifications_enabled);
}

// ───────────────────────────────────────────────────────────────────
// Scenario 4 — spatial `GeoPoint` + `extensions = ["postgis"]`
// + `sync_models` ordering
// ───────────────────────────────────────────────────────────────────

#[cfg(feature = "spatial")]
#[model(table = "t10_places", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: djogi::GeoPoint,
}

#[cfg(feature = "spatial")]
fn place_for_insert(name: &str, lat: f64, lon: f64) -> Place {
    Place {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        name: name.into(),
        location: djogi::GeoPoint::new(lat, lon).unwrap(),
    }
}

#[cfg(feature = "spatial")]
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Place])]
async fn spatial_field_extension_provisioned_first(mut ctx: djogi::DjogiContext) {
    // The table exists — provable only because PostGIS was provisioned
    // BEFORE `sync_models` (the table's `geography(Point, 4326)`
    // column type would otherwise fail with "type does not exist").
    let exists: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_class WHERE relname = 't10_places'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(exists, 1);

    // The column resolves to a geography type — `udt_name` reports
    // `geography` for any GEOGRAPHY column regardless of the type
    // modifier (Point, LineString, etc.).
    let udt: String = ctx
        .raw_scalar(
            "SELECT udt_name FROM information_schema.columns \
             WHERE table_name = 't10_places' AND column_name = 'location'",
            &[],
        )
        .await
        .expect("information_schema lookup");
    assert_eq!(udt, "geography");

    // Round-trip a GeoPoint and run a spatial query — proves the GIST
    // index emitted alongside the table is exercisable.
    let sfo = djogi::GeoPoint::new(37.6189, -122.3750).unwrap();
    let _p = Place::create(&mut ctx, place_for_insert("SFO", 37.6189, -122.3750))
        .await
        .expect("Place::create against PostGIS-backed table");

    let nearby = Place::objects()
        .filter(|p| p.location().within_km(sfo, 50.0))
        .fetch_all(&mut ctx)
        .await
        .expect("within_km query against sync_models-created spatial schema");
    assert_eq!(nearby.len(), 1, "SFO must match within_km(SFO, 50)");
}

// ───────────────────────────────────────────────────────────────────
// Scenario 5 — M2M through-model included in sync_models
// ───────────────────────────────────────────────────────────────────

#[model(table = "t10_tags", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Tag {
    pub label: String,
}

#[model(table = "t10_posts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
}

/// The junction model. `through` flag declares it's an M2M join
/// table. Both FKs target the M2M endpoints — descriptor projection
/// emits both, and `sync_models`'s pre-flight FK check accepts the
/// pair because both endpoints are in the supplied list.
#[model(table = "t10_post_tags", pk = HeerId, through, no_default)]
#[derive(Debug, Clone)]
pub struct PostTag {
    pub post_id: ForeignKey<Post>,
    pub tag_id: ForeignKey<Tag>,
}

fn post_tag_for_insert(post: &Post, tag: &Tag) -> PostTag {
    PostTag {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        post_id: ForeignKey::new(post.id),
        tag_id: ForeignKey::new(tag.id),
    }
}

djogi::many_to_many!(
    Post,
    Tag,
    through = PostTag,
    this_fk = post_id,
    that_fk = tag_id,
    relation = "tags"
);

#[djogi::djogi_test(sync_models = [Tag, Post, PostTag])]
async fn m2m_through_model_materialises_all_three_tables(mut ctx: djogi::DjogiContext) {
    for tbl in ["t10_tags", "t10_posts", "t10_post_tags"] {
        let exists: i64 = ctx
            .raw_scalar(
                "SELECT count(*)::bigint FROM pg_class WHERE relname = $1",
                &[&tbl],
            )
            .await
            .unwrap();
        assert_eq!(exists, 1, "table {tbl} must be created by sync_models");
    }

    // The post_tags junction has FKs to both endpoints; verify both.
    let fk_count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_constraint c \
             JOIN pg_class src ON src.oid = c.conrelid \
             WHERE c.contype = 'f' AND src.relname = 't10_post_tags'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        fk_count, 2,
        "junction table must have one FK per endpoint (post + tag)"
    );

    // Round-trip the M2M relation through the typed accessor — proves
    // the emitted schema lets the M2M trait method execute its JOIN.
    let post = Post::create(
        &mut ctx,
        Post {
            title: "first".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let tag = Tag::create(
        &mut ctx,
        Tag {
            label: "rust".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    PostTag::create(&mut ctx, post_tag_for_insert(&post, &tag))
        .await
        .expect("PostTag::create");

    let tags = post
        .tags(&mut ctx)
        .await
        .expect("M2M accessor returns rows");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].label, "rust");
}

// ───────────────────────────────────────────────────────────────────
// Scenario 6 — FK cycle (A → B → A)
// ───────────────────────────────────────────────────────────────────

/// Mutually-referencing pair: each side carries a nullable FK at the
/// other. The migration engine's `toposort_add_tables` detects the
/// cycle, strips the inline FK from one side's `CREATE TABLE`, and
/// follows up with a standalone `AddForeignKey`. Both FKs are
/// `Option<...>` so the first-side row can INSERT with NULL and the
/// second-side row populates the back-pointer; this exercises the
/// cycle-breaking path without needing `SET CONSTRAINTS DEFERRED`.
#[model(table = "t10_users_cycle", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct CycleUser {
    pub name: String,
    pub team_id: Option<ForeignKey<CycleTeam>>,
}

#[model(table = "t10_teams_cycle", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct CycleTeam {
    pub name: String,
    pub lead_user_id: Option<ForeignKey<CycleUser>>,
}

fn cycle_user_for_insert(name: &str, team: Option<&CycleTeam>) -> CycleUser {
    CycleUser {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        name: name.into(),
        team_id: team.map(|t| ForeignKey::new(t.id)),
    }
}

fn cycle_team_for_insert(name: &str, lead: Option<&CycleUser>) -> CycleTeam {
    CycleTeam {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        name: name.into(),
        lead_user_id: lead.map(|u| ForeignKey::new(u.id)),
    }
}

#[djogi::djogi_test(sync_models = [CycleUser, CycleTeam])]
async fn fk_cycle_breaks_via_followup_constraint(mut ctx: djogi::DjogiContext) {
    // Both tables exist.
    for tbl in ["t10_users_cycle", "t10_teams_cycle"] {
        let exists: i64 = ctx
            .raw_scalar(
                "SELECT count(*)::bigint FROM pg_class WHERE relname = $1",
                &[&tbl],
            )
            .await
            .unwrap();
        assert_eq!(exists, 1);
    }

    // Total FK constraints across the cycle peers = 2 (one per side).
    let total_fks: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_constraint c \
             JOIN pg_class src ON src.oid = c.conrelid \
             WHERE c.contype = 'f' \
               AND src.relname IN ('t10_users_cycle', 't10_teams_cycle')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(total_fks, 2, "cycle peers must each have one FK");

    // Insertions into both sides round-trip cleanly.
    let user = CycleUser::create(&mut ctx, cycle_user_for_insert("alice", None))
        .await
        .expect("CycleUser::create");
    let team = CycleTeam::create(&mut ctx, cycle_team_for_insert("core", Some(&user)))
        .await
        .expect("CycleTeam::create with lead user reference");

    // Now close the loop on the user side.
    let mut user = user;
    user.team_id = Some(ForeignKey::new(team.id));
    user.save(&mut ctx)
        .await
        .expect("CycleUser::save closes the cycle");
}

// ───────────────────────────────────────────────────────────────────
// Scenario 7 — FK target NOT in sync_models — clean runtime error
// ───────────────────────────────────────────────────────────────────

#[model(table = "t10_categories_orphan", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OrphanCategory {
    pub name: String,
}

#[model(table = "t10_widgets_orphan", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct OrphanWidget {
    pub category_id: ForeignKey<OrphanCategory>,
    pub name: String,
}

/// We bypass the macro's `sync_models` argument here so we can
/// inspect the returned error directly. The `#[djogi_test]`
/// bootstrap still gives us a per-test database; we then call
/// `djogi::testing::sync_models` ourselves with a list that omits
/// the FK target.
#[djogi::djogi_test]
async fn fk_target_missing_returns_named_runtime_error(mut ctx: djogi::DjogiContext) {
    let err = djogi::testing::sync_models(
        &mut ctx,
        &[<OrphanWidget as djogi::prelude::Model>::descriptor()],
    )
    .await
    .expect_err("sync_models must reject a list whose FK target is missing");

    let msg = format!("{err}");
    assert!(
        msg.contains("OrphanWidget"),
        "error must name the source model: {msg}"
    );
    assert!(
        msg.contains("category_id"),
        "error must name the FK column: {msg}"
    );
    assert!(
        msg.contains("OrphanCategory"),
        "error must name the missing target: {msg}"
    );
    assert!(
        msg.contains("sync_models"),
        "error must call out the sync_models contract: {msg}"
    );
}

// ───────────────────────────────────────────────────────────────────
// Scenario 8 — index emission on sync_models'd table (T3 IndexSpec)
// ───────────────────────────────────────────────────────────────────

/// One model with a model-level GIN index expressed via
/// `using = "gin"`. We assert the resulting index access method
/// (`pg_am.amname`) matches the descriptor — proves
/// `sync_models` routes through the same SQL emitter the migration
/// engine uses for `IndexSpec`, not a parallel index emitter.
#[model(table = "t10_documents", pk = HeerId, indexes(
    index(fields = [tags], using = "gin"),
))]
#[derive(Debug, Clone)]
pub struct Document {
    pub title: String,
    pub tags: Vec<String>,
}

#[djogi::djogi_test(sync_models = [Document])]
async fn index_spec_routes_through_migration_engine(mut ctx: djogi::DjogiContext) {
    // Look up the access method for any non-PK index on the table.
    // Filter out the PK by name — the PK index is automatically named
    // `<table>_pkey`. Anything else is one of our declared indexes.
    let am: Option<String> = ctx
        .raw_scalar(
            "SELECT am.amname \
             FROM pg_class c \
             JOIN pg_index i ON i.indexrelid = c.oid \
             JOIN pg_class t ON t.oid = i.indrelid \
             JOIN pg_am am ON am.oid = c.relam \
             WHERE t.relname = 't10_documents' \
               AND c.relname <> 't10_documents_pkey' \
             LIMIT 1",
            &[],
        )
        .await
        .ok();
    assert_eq!(
        am.as_deref(),
        Some("gin"),
        "model-declared GIN index must be created via the migration engine's SQL emitter"
    );

    // The descriptor declares one model-level index; assert that
    // shape is preserved through `sync_models`. (Direct descriptor
    // inspection — does not depend on the live DB.)
    let descriptor = <Document as djogi::prelude::Model>::descriptor();
    let gin_count = descriptor
        .indexes
        .iter()
        .filter(|spec| {
            matches!(spec.kind, IndexKind::NonUnique)
                && spec.index_type == djogi::descriptor::IndexType::Gin
                && matches!(spec.target, IndexTarget::Columns(_))
        })
        .count();
    assert_eq!(
        gin_count, 1,
        "Document descriptor must declare exactly one GIN index"
    );
}
