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
