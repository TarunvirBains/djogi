//! Phase 6 integration tests — descriptor population (T2) and query-surface
//! IR shape (T3).
//!
//! ## T2: Descriptor population for GeoPoint fields
//!
//! Verifies that the `#[derive(Model)]` / `#[model]` macro pair correctly
//! populates the `ModelDescriptor` when a model declares a `GeoPoint` field:
//!
//! 1. `geography_sql_type_on_location_field` — the `location` field descriptor
//!    has `sql_type == FieldSqlType::Geography { srid: 4326 }`.
//! 2. `gist_index_in_descriptor_for_geopoint_field` — `ModelDescriptor::indexes`
//!    contains exactly one `IndexSpec` for the `location` column, with
//!    `index_type == IndexType::Gist`.
//! 3. `gist_index_name_follows_convention` — the index name matches the
//!    `<table>_<column>_gix` naming convention.
//! 4. `non_spatial_model_has_empty_indexes` — a plain non-spatial model has no
//!    entries in `ModelDescriptor::indexes` (regression guard).
//!
//! ## T3: Spatial query-surface IR shape
//!
//! Verifies the Condition / OrderExpr routing without a live database:
//!
//! 5. `within_km_returns_condition_expr` — `within_km` returns `Condition::Expr`
//!    for IR uniformity with the Phase 4 expression substrate.
//! 6. `order_by_distance_returns_order_expr` — `order_by_distance` returns an
//!    `OrderExpr` that the `order_by` closure accepts.
//!
//! All T2 and T3 tests are DB-free. Live-PostGIS CRUD tests are T4's scope.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

/// Primary spatial model under test: one GeoPoint field.
///
/// `no_default` is required because `GeoPoint` does not implement `Default`
/// (there is no semantically meaningful default geographic coordinate). The
/// `#[model]` macro's `Default` derivation is skipped; struct-update syntax
/// is unavailable on `Place`, which is acceptable for a descriptor-inspection
/// test that never constructs instances.
#[cfg(feature = "spatial")]
#[allow(dead_code)]
#[model(table = "places", no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: djogi::GeoPoint,
}

/// Regression model: no GeoPoint fields — indexes slice must be empty.
#[allow(dead_code)]
#[model(table = "non_spatial_items")]
#[derive(Debug, Clone)]
pub struct NonSpatialItem {
    pub label: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The `location` field on `Place` must have `sql_type == Geography { srid: 4326 }`.
#[cfg(feature = "spatial")]
#[test]
fn geography_sql_type_on_location_field() {
    let desc = Place::descriptor();
    let field = desc
        .fields
        .iter()
        .find(|f| f.name == "location")
        .expect("location field must be present in Place descriptor");

    assert!(
        matches!(field.sql_type, FieldSqlType::Geography { srid: 4326 }),
        "expected Geography {{ srid: 4326 }}, got {:?}",
        field.sql_type
    );
}

/// `Place::descriptor().indexes` must contain exactly one entry for the
/// `location` column, with `index_type == IndexType::Gist`.
#[cfg(feature = "spatial")]
#[test]
fn gist_index_in_descriptor_for_geopoint_field() {
    let desc = Place::descriptor();
    let gix = desc
        .indexes
        .iter()
        .find(|idx| idx.columns == ["location"])
        .expect("GiST index for `location` must be present in Place::descriptor().indexes");

    assert_eq!(
        gix.index_type,
        IndexType::Gist,
        "spatial index must use GiST; got {:?}",
        gix.index_type
    );
    assert!(
        !gix.unique,
        "spatial GiST index must not be unique — spatial indexes are never unique constraints"
    );
}

/// The GiST index name must follow the `<table>_<column>_gix` convention.
///
/// This convention is consumed by the Phase 7 migration emitter when it
/// produces `CREATE INDEX <name> ON <table> USING GIST (<col>)`. Enforcing
/// the name here keeps the emitter and the descriptor in sync.
#[cfg(feature = "spatial")]
#[test]
fn gist_index_name_follows_convention() {
    let desc = Place::descriptor();
    let gix = desc
        .indexes
        .iter()
        .find(|idx| idx.columns == ["location"])
        .expect("GiST index for `location` must be present");

    assert_eq!(
        gix.name, "places_location_gix",
        "index name must be '<table>_<column>_gix'; got '{}'",
        gix.name
    );
}

/// A non-spatial model must have an empty `indexes` slice — regression guard
/// ensuring T2's index-building logic does not accidentally emit entries for
/// models with no GeoPoint fields.
#[test]
fn non_spatial_model_has_empty_indexes() {
    let desc = NonSpatialItem::descriptor();
    assert!(
        desc.indexes.is_empty(),
        "non-spatial model must not have any IndexSpec entries; found: {:?}",
        desc.indexes
    );
}

// ---------------------------------------------------------------------------
// T3: Spatial query-surface IR shape (DB-free)
// ---------------------------------------------------------------------------

/// `FieldRef<Place, GeoPoint>::within_km(center, km)` must return
/// `Condition::Expr` so the spatial predicate routes through the Phase 4
/// expression substrate rather than introducing a parallel `Condition::Spatial`
/// arm. IR uniformity means spatial predicates compose with `filter`,
/// `filter_expr`, `And`/`Or`, and correlated subqueries without special-casing
/// in the condition emitter.
#[cfg(feature = "spatial")]
#[test]
fn within_km_returns_condition_expr() {
    let center = djogi::GeoPoint::new(37.7749, -122.4194).unwrap();
    // Capture the Condition via the filter closure — the idiomatic API surface.
    // The queryset is lazy; no DB call happens here.
    let mut captured: Option<djogi::query::condition::Condition> = None;
    let _qs = Place::objects().filter(|p| {
        let cond = p.location().within_km(center, 5.0);
        captured = Some(cond.clone());
        cond
    });
    let cond = captured.unwrap();
    assert!(
        matches!(cond, djogi::query::condition::Condition::Expr(_)),
        "within_km must return Condition::Expr for IR uniformity; got {cond:?}"
    );
}

/// `FieldRef<Place, GeoPoint>::order_by_distance(center)` must return an
/// `OrderExpr` that the `QuerySet::order_by` closure accepts.
///
/// The exact SQL shape (`ST_Distance(...) ASC, id ASC`) is tested in
/// `djogi/src/expr/spatial.rs` and `djogi/src/query/order.rs` unit tests.
/// Here we verify the type-level routing: the closure return type is accepted
/// by `order_by` without a `.clone()` or `vec![...]` wrapper.
#[cfg(feature = "spatial")]
#[test]
fn order_by_distance_returns_order_expr() {
    let center = djogi::GeoPoint::new(37.7749, -122.4194).unwrap();
    // `order_by_distance` produces an `OrderExpr` accepted by `order_by`.
    // The queryset is lazy; no DB call happens here.
    let _qs = Place::objects().order_by(|p| p.location().order_by_distance(center));
    // If this compiles and doesn't panic, the OrderExpr routing is correct.
}

/// A complete `QuerySet` with both `within_km` filter and `order_by_distance`
/// ordering must compile and produce a queryset without panicking.
///
/// This is the IR composition smoke test: it exercises the full path from
/// `FieldRef` method → `Condition::Expr(ExprNode::Spatial)` → `QuerySet`
/// accumulation. SQL text assertion is T4's scope (requires a live PostGIS
/// instance for the full SELECT round-trip); here we verify the IR composes
/// without panics.
#[cfg(feature = "spatial")]
#[test]
fn queryset_with_spatial_filter_and_ordering_composes_without_panic() {
    let center = djogi::GeoPoint::new(37.7749, -122.4194).unwrap();
    // Neither filter nor order_by execute SQL — they are lazy accumulators.
    let _qs = Place::objects()
        .filter(|p| p.location().within_km(center, 50.0))
        .order_by(|p| p.location().order_by_distance(center));
    // Reaching this line means both methods type-checked and the queryset
    // accumulated the condition and ordering without panicking.
}

// ---------------------------------------------------------------------------
// T4: Live PostGIS CRUD and query semantics
// ---------------------------------------------------------------------------
//
// These tests require a live PostgreSQL 18 instance with the PostGIS 3.x
// extension installable by the test role. `setup_phase6` provisions the
// extension and the `places` table inline via `ctx.raw_ddl` / `ctx.raw_execute`
// (idempotent — safe to call at the start of every test).
//
// ## Tests
//
// - `geopoint_crud_round_trip`: Place::create → Place::get proves the EWKB
//   codec round-trips GEOGRAPHY(Point, 4326) through Postgres without coord drift.
// - `within_km_filters_correctly`: seeds SFO, OAK, JFK; filters to within 50km
//   of SFO; asserts exactly SFO + OAK land in the result, JFK does not.
// - `order_by_distance_is_deterministic`: seeds two equidistant points from
//   center=(0,0); runs order_by_distance twice; asserts identical ID order both
//   runs and that the smaller PK comes first (PK tiebreak).

/// Construct a `Place` with explicit sentinel values for framework fields.
///
/// `Place` has `no_default` because `GeoPoint` has no meaningful default.
/// Sentinel values for `id`, `created_at`, `updated_at` are overwritten by
/// the database via `RETURNING *` and column defaults — the values here are
/// never persisted.
#[cfg(feature = "spatial")]
fn place(name: &str, lat: f64, lon: f64) -> Place {
    Place {
        id: djogi::HeerId::from_i64(0).expect("0 is a valid HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: name.to_string(),
        location: djogi::GeoPoint::new(lat, lon).unwrap(),
    }
}

/// Provision PostGIS and the `places` table in the per-test database.
///
/// Called at the start of each T4 live-DB test. All statements are guarded
/// with `IF NOT EXISTS` so the helper is idempotent across multiple calls
/// within the same per-test DB lifetime.
///
/// Two separate `raw_ddl` calls are used because `raw_ddl` wraps
/// `batch_execute` (the Postgres simple-query protocol), which accepts
/// multi-statement strings. The extension must be installed before the table
/// DDL runs because `GEOGRAPHY` is a PostGIS type.
#[cfg(feature = "spatial")]
async fn setup_phase6(ctx: &mut djogi::DjogiContext) {
    // Install the PostGIS extension. This must succeed before any GEOGRAPHY
    // column or ST_* function is used. `raw_ddl` uses the simple-query
    // protocol (batch_execute), which accepts DDL that prepared statements
    // cannot handle (e.g., CREATE EXTENSION).
    ctx.raw_ddl("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
        .expect("install postgis extension");

    // Create the places table and its spatial index in one batch.
    // Column order matches the #[model] injection order: id, created_at,
    // updated_at, then user-defined columns (name, location).
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS places (
             id         BIGINT       PRIMARY KEY DEFAULT generate_id(),
             created_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
             updated_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
             name       TEXT         NOT NULL,
             location   GEOGRAPHY(Point, 4326) NOT NULL
         );
         CREATE INDEX IF NOT EXISTS places_location_gix
             ON places USING GIST(location);",
    )
    .await
    .expect("create places table and spatial index");
}

/// EWKB codec round-trip through a live Postgres GEOGRAPHY(Point, 4326) column.
///
/// Creates a Place row via the ORM path (`Place::create`), reloads it via
/// `Place::get`, and asserts that both coordinates match the original within
/// 1e-9 degrees. This exercises the full path:
///
///   GeoPoint → to_ewkb_bytes → ToSql → INSERT RETURNING * → FromSql →
///   from_ewkb_bytes → GeoPoint
///
/// A 1e-9 tolerance covers any f64 representation noise; PostGIS stores
/// GEOGRAPHY as IEEE 754 double-precision, so the round-trip should be exact.
#[cfg(feature = "spatial")]
#[djogi::djogi_test]
async fn geopoint_crud_round_trip(mut ctx: djogi::DjogiContext) {
    setup_phase6(&mut ctx).await;

    let sfo = djogi::GeoPoint::new(37.6189, -122.3750).unwrap();
    let created = Place::create(&mut ctx, place("SFO", 37.6189, -122.3750))
        .await
        .expect("Place::create must succeed");

    let reloaded = Place::get(&mut ctx, created.id)
        .await
        .expect("Place::get must find the just-created row");

    assert!(
        (reloaded.location.lat - sfo.lat).abs() < 1e-9,
        "latitude drifted: expected {}, got {}",
        sfo.lat,
        reloaded.location.lat
    );
    assert!(
        (reloaded.location.lon - sfo.lon).abs() < 1e-9,
        "longitude drifted: expected {}, got {}",
        sfo.lon,
        reloaded.location.lon
    );
}

/// `within_km` filters to only those rows within the specified distance.
///
/// Seeds three airports:
/// - SFO (San Francisco Intl): 37.6189°N, 122.3750°W
/// - OAK (Oakland Intl): 37.7213°N, 122.2207°W — approximately 20 km from SFO
/// - JFK (John F. Kennedy Intl): 40.6413°N, 73.7781°W — approximately 4151 km from SFO
///
/// A filter of `within_km(sfo, 50.0)` must return exactly SFO and OAK; JFK
/// must be absent. The 50 km radius is wide enough to tolerate PostGIS's
/// geodetic distance calculation (which differs slightly from Haversine) while
/// remaining far below the SFO-JFK distance.
#[cfg(feature = "spatial")]
#[djogi::djogi_test]
async fn within_km_filters_correctly(mut ctx: djogi::DjogiContext) {
    setup_phase6(&mut ctx).await;

    Place::create(&mut ctx, place("SFO", 37.6189, -122.3750))
        .await
        .expect("create SFO");
    Place::create(&mut ctx, place("OAK", 37.7213, -122.2207))
        .await
        .expect("create OAK");
    Place::create(&mut ctx, place("JFK", 40.6413, -73.7781))
        .await
        .expect("create JFK");

    let sfo_center = djogi::GeoPoint::new(37.6189, -122.3750).unwrap();
    let nearby = Place::objects()
        .filter(|p| p.location().within_km(sfo_center, 50.0))
        .fetch_all(&mut ctx)
        .await
        .expect("within_km query must succeed");

    assert_eq!(
        nearby.len(),
        2,
        "expected exactly 2 airports within 50km of SFO (SFO + OAK), got {}; names: {:?}",
        nearby.len(),
        nearby.iter().map(|p| p.name.as_str()).collect::<Vec<_>>()
    );

    let names: Vec<&str> = nearby.iter().map(|p| p.name.as_str()).collect();
    assert!(
        names.contains(&"SFO"),
        "SFO must be in the within-50km result; got {:?}",
        names
    );
    assert!(
        names.contains(&"OAK"),
        "OAK must be in the within-50km result; got {:?}",
        names
    );
    assert!(
        !names.contains(&"JFK"),
        "JFK must NOT be in the within-50km result; got {:?}",
        names
    );
}

/// `order_by_distance` produces a stable, deterministic ordering even when
/// rows are equidistant from the center point.
///
/// Seeds two points at equal geodetic distance from (0, 0):
/// - north: (1.0, 0.0) — approximately 111 km north
/// - south: (-1.0, 0.0) — approximately 111 km south
///
/// Runs the `order_by_distance` query twice. The results must be identical
/// both times. Furthermore, the row with the smaller primary key must appear
/// first — the PK tiebreak appended unconditionally by `order_by_distance`
/// makes the ordering stable when distances are equal.
///
/// Note: the test creates north before south, so north's PK will be smaller.
/// The assertion uses `std::cmp::min` to remain correct if ID generation
/// order ever changes.
#[cfg(feature = "spatial")]
#[djogi::djogi_test]
async fn order_by_distance_is_deterministic(mut ctx: djogi::DjogiContext) {
    setup_phase6(&mut ctx).await;

    let center = djogi::GeoPoint::new(0.0, 0.0).unwrap();

    // Create north first — its PK will be lower (IDs are time-ordered).
    let north_row = Place::create(&mut ctx, place("north", 1.0, 0.0))
        .await
        .expect("create north");
    let south_row = Place::create(&mut ctx, place("south", -1.0, 0.0))
        .await
        .expect("create south");

    // Run the distance ordering twice.
    let first = Place::objects()
        .order_by(|p| p.location().order_by_distance(center))
        .fetch_all(&mut ctx)
        .await
        .expect("first order_by_distance query");

    let second = Place::objects()
        .order_by(|p| p.location().order_by_distance(center))
        .fetch_all(&mut ctx)
        .await
        .expect("second order_by_distance query");

    let first_ids: Vec<djogi::HeerId> = first.iter().map(|p| p.id).collect();
    let second_ids: Vec<djogi::HeerId> = second.iter().map(|p| p.id).collect();

    assert_eq!(
        first_ids, second_ids,
        "order_by_distance must return identical ordering across repeated queries; \
         first={first_ids:?}, second={second_ids:?}"
    );

    // The row with the smaller PK must come first (PK tiebreak).
    let expected_first = std::cmp::min(north_row.id, south_row.id);
    assert_eq!(
        first_ids[0], expected_first,
        "the smaller PK must appear first when distances are equal; \
         expected {expected_first:?}, got {:?}",
        first_ids[0]
    );
}
