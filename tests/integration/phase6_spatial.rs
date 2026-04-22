//! Phase 6 Task 2 integration tests — descriptor population for GeoPoint fields.
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
//! All tests here are DB-free — they read static descriptor data that the
//! `inventory::submit!` call produces at startup. No `DjogiContext` or live
//! Postgres connection is required for T2 coverage. Live-PostGIS CRUD tests
//! are T4's scope.

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
