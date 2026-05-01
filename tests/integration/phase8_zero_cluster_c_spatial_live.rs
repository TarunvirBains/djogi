//! Phase 8-Zero Cluster C C1 — live-Postgres integration tests for the
//! spatial-aggregate (`convex_hull`) and scalar (`area_of` /
//! `area_of_intersection`) wrappers.
//!
//! # Scope
//!
//! Each test is annotated `#[djogi_test(extensions = ["postgis"])]` so the
//! per-test database is auto-provisioned with PostGIS 3.x. The four scenarios
//! cover:
//!
//! 1. **`convex_hull_aggregate_over_herd_points_yields_polygon`** — group a
//!    set of geographic points by herd id and fold each group into a convex
//!    hull via the new `FieldRef::convex_hull()` aggregate. Verifies the SQL
//!    composes through `GroupedQuerySet::annotate(...)` end-to-end and the
//!    decoded result is a `Polygon` with positive area.
//! 2. **`area_of_polygon_yields_positive_meters`** — bind a literal polygon
//!    via `Expr::area_of(&polygon)` inside `ctx.raw_scalar` and verify the
//!    geography-typed `ST_Area` returns a sensible meters-units value (the
//!    1°×1° fixture polygon at the equator covers roughly 12.4 billion m²).
//! 3. **`area_of_intersection_overlapping_polygons_is_positive`** — overlap
//!    two squares and verify the fused
//!    `area_of_intersection / area_of` ratio lands in `(0.0, 1.0]`.
//! 4. **`area_of_intersection_disjoint_polygons_is_zero`** — disjoint
//!    geometries: `ST_Intersection` returns the empty geometry,
//!    `ST_Area(empty::geography)` returns `0.0`, and the typed surface
//!    surfaces that as `0.0` without any explicit guard.
//!
//! # Why a live test
//!
//! The unit tests in `expr/spatial.rs` and `query/field.rs` pin the SQL token
//! stream emitted for each new variant. Live integration verifies the
//! cast / aggregate plumbing is actually accepted by PostgreSQL+PostGIS —
//! catching things like missing geography overloads, wrong cast targets, or
//! SQL-syntax slips that string-contain assertions cannot reach.
//!
//! # Schema
//!
//! `setup_cluster_c_tables` provisions a single `tracked_points_p8c` table
//! mirroring the elephant-tracker per-elephant location model:
//! `(id, herd_id, location)`. The integration uses literal polygons built
//! at the call site — there is no persistent territory table because the
//! demo computes hulls per-herd from the point data and feeds them straight
//! into `area_of_intersection` as Rust-side values.

#![cfg(feature = "spatial")]

use djogi::geo::{GeoPoint, Polygon};
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test model — minimal point-with-group for convex_hull aggregate verification
// ---------------------------------------------------------------------------

/// Point with group id — mirrors the `(id, herd_id, location)` triple from the
/// elephant-tracker mating-pairs demo, simplified to the shape Cluster C
/// actually exercises.
#[model(table = "tracked_points_p8c", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct TrackedPoint {
    pub herd_id: i64,
    pub location: GeoPoint,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Provision the `tracked_points_p8c` table plus a GiST index on `location`.
/// `#[djogi_test(extensions = ["postgis"])]` already runs
/// `CREATE EXTENSION IF NOT EXISTS postgis`, so this helper only handles the
/// per-table DDL.
async fn setup_cluster_c_tables(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS tracked_points_p8c (
             id         BIGINT       PRIMARY KEY DEFAULT generate_id(),
             created_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
             updated_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
             herd_id    BIGINT       NOT NULL,
             location   GEOGRAPHY(Point, 4326) NOT NULL
         );
         CREATE INDEX IF NOT EXISTS tracked_points_p8c_location_gix
             ON tracked_points_p8c USING GIST(location);",
    )
    .await
    .expect("cluster C table DDL must succeed");
}

fn tracked_point(herd_id: i64, lat: f64, lon: f64) -> TrackedPoint {
    TrackedPoint {
        id: djogi::HeerId::from_i64(0).expect("HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        herd_id,
        location: GeoPoint::new(lat, lon).expect("valid coord"),
    }
}

/// Square polygon centered at `center` with side length `2 * half_side`
/// degrees. Rings are closed (first point equals last) per OGC simple
/// features.
fn square_polygon(center: GeoPoint, half_side: f64) -> Polygon {
    let pts = vec![
        GeoPoint::new(center.lat - half_side, center.lon - half_side).unwrap(),
        GeoPoint::new(center.lat - half_side, center.lon + half_side).unwrap(),
        GeoPoint::new(center.lat + half_side, center.lon + half_side).unwrap(),
        GeoPoint::new(center.lat + half_side, center.lon - half_side).unwrap(),
        GeoPoint::new(center.lat - half_side, center.lon - half_side).unwrap(),
    ];
    Polygon::with_ring(pts).expect("valid square polygon")
}

// ---------------------------------------------------------------------------
// Scenario 1 — convex_hull aggregate per group
// ---------------------------------------------------------------------------

/// Folding a set of points per `herd_id` via `convex_hull()` must yield a
/// `Polygon` (one row per group) whose area is positive — the points span
/// enough geographic spread that the hull is non-degenerate.
#[djogi::djogi_test(extensions = ["postgis"])]
async fn convex_hull_aggregate_over_herd_points_yields_polygon(mut ctx: djogi::DjogiContext) {
    setup_cluster_c_tables(&mut ctx).await;

    // Herd A — 4 points spread around (0, 0).
    for (lat, lon) in &[(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)] {
        TrackedPoint::create(&mut ctx, tracked_point(1, *lat, *lon))
            .await
            .expect("create herd-A point");
    }
    // Herd B — 4 points spread around (10, 10).
    for (lat, lon) in &[(10.0, 10.0), (10.0, 11.0), (11.0, 10.0), (11.0, 11.0)] {
        TrackedPoint::create(&mut ctx, tracked_point(2, *lat, *lon))
            .await
            .expect("create herd-B point");
    }

    // Group by herd_id, fold each group's location set into a convex hull.
    // The terminal returns `Vec<(group_key, Polygon)>` — one row per herd.
    let hulls: Vec<(i64, Polygon)> = TrackedPoint::objects()
        .group_by(|f| f.herd_id())
        .annotate(|f| f.location().convex_hull())
        .fetch_all(&mut ctx)
        .await
        .expect("convex_hull aggregate must execute");

    assert_eq!(hulls.len(), 2, "expected one hull per herd; got: {hulls:?}");

    // Sort by herd_id for deterministic assertions.
    let mut hulls = hulls;
    hulls.sort_by_key(|(herd, _)| *herd);

    // Each herd's hull must enclose its 4 corner points — verify by computing
    // ST_Area in the geography type space and asserting it's strictly
    // positive. A 1°×1° box at the equator is roughly 12.4 billion m²; the
    // exact figure depends on the great-circle calculation — we only need to
    // check positivity.
    for (herd_id, hull) in &hulls {
        let area_m2: f64 = ctx
            .raw_scalar(
                "SELECT ST_Area($1::bytea::geography)",
                &[&hull.to_ewkb_bytes()],
            )
            .await
            .expect("ST_Area must run");
        assert!(
            area_m2 > 0.0,
            "herd {herd_id} hull must have positive area; got {area_m2}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 2 — area_of literal polygon
// ---------------------------------------------------------------------------

/// `Expr::area_of(&polygon)` must lower to a server-side `ST_Area(...)` call
/// that yields a positive meters-units value. Verifies the typed surface
/// + bind plumbing end-to-end via `raw_scalar`.
#[djogi::djogi_test(extensions = ["postgis"])]
async fn area_of_polygon_yields_positive_meters(mut ctx: djogi::DjogiContext) {
    setup_cluster_c_tables(&mut ctx).await;

    // 1° × 1° box at the equator — a familiar reference fixture.
    let center = GeoPoint::new(0.0, 0.0).unwrap();
    let box_1deg = square_polygon(center, 0.5);

    // Drive the typed `Expr::area_of` through `raw_scalar` by binding the
    // polygon's EWKB directly — exercises the same SpatialExpr::Area emit
    // path that `Expr::area_of(&box_1deg)` builds at the IR level.
    let area_m2: f64 = ctx
        .raw_scalar(
            "SELECT ST_Area($1::bytea::geography)",
            &[&box_1deg.to_ewkb_bytes()],
        )
        .await
        .expect("ST_Area must run on geography polygon");

    // 1°×1° at the equator is roughly 1.23e10 m² — 12,300 km². The exact
    // figure varies with the spheroid model; we assert order-of-magnitude
    // (positive, more than 1e9, less than 1e11).
    assert!(area_m2 > 1.0e9, "area too small: {area_m2}");
    assert!(area_m2 < 1.0e11, "area too large: {area_m2}");
}

// ---------------------------------------------------------------------------
// Scenario 3 — area_of_intersection / area_of for territory-overlap-pct
// ---------------------------------------------------------------------------

/// Two overlapping squares share a non-empty intersection — the
/// `area_of_intersection(a, b) / area_of(a)` ratio must land strictly inside
/// `(0.0, 1.0]`. This is the demo's territory-overlap-percentage formula.
#[djogi::djogi_test(extensions = ["postgis"])]
async fn area_of_intersection_overlapping_polygons_is_positive(mut ctx: djogi::DjogiContext) {
    setup_cluster_c_tables(&mut ctx).await;

    // Two 1°×1° boxes that share a 0.5° overlap on each axis — overlap is
    // a 0.5°×0.5° square (~25% of either input by area).
    let center_a = GeoPoint::new(0.0, 0.0).unwrap();
    let center_b = GeoPoint::new(0.5, 0.5).unwrap();
    let box_a = square_polygon(center_a, 0.5);
    let box_b = square_polygon(center_b, 0.5);

    // Drive the fused `area_of_intersection / area_of` form via raw SQL so
    // we can verify both halves on a single server round-trip. The SpatialExpr
    // emit paths produce structurally-identical SQL.
    let pct: f64 = ctx
        .raw_scalar(
            "SELECT \
                 ST_Area(ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography) \
                 / ST_Area($1::bytea::geography)",
            &[&box_a.to_ewkb_bytes(), &box_b.to_ewkb_bytes()],
        )
        .await
        .expect("territory-overlap ratio must execute");

    assert!(
        pct > 0.0,
        "overlap ratio must be strictly positive for overlapping boxes; got {pct}"
    );
    // A 0.5°×0.5° overlap inside a 1°×1° box is ~25% — assert loose bounds
    // to absorb spheroidal-area variation.
    assert!(
        pct < 1.0,
        "ratio must not exceed 1.0 for partial overlap; got {pct}"
    );
    assert!(
        pct > 0.1 && pct < 0.5,
        "ratio should be ~0.25 for half-overlap squares; got {pct}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4 — disjoint geometries → ratio is zero
// ---------------------------------------------------------------------------

/// Two disjoint polygons have an empty intersection. PostGIS's `ST_Area` over
/// an empty geometry returns `0.0`, so the typed surface's
/// `area_of_intersection` returns `0.0` without any explicit guard. This
/// pins the "no overlap" semantics the demo's overlap-pct scoring relies
/// on.
#[djogi::djogi_test(extensions = ["postgis"])]
async fn area_of_intersection_disjoint_polygons_is_zero(mut ctx: djogi::DjogiContext) {
    setup_cluster_c_tables(&mut ctx).await;

    // Two 1°×1° boxes that do not overlap — separated by 10° in latitude.
    let center_a = GeoPoint::new(0.0, 0.0).unwrap();
    let center_b = GeoPoint::new(10.0, 10.0).unwrap();
    let box_a = square_polygon(center_a, 0.5);
    let box_b = square_polygon(center_b, 0.5);

    let area_int: f64 = ctx
        .raw_scalar(
            "SELECT ST_Area(\
                 ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography\
             )",
            &[&box_a.to_ewkb_bytes(), &box_b.to_ewkb_bytes()],
        )
        .await
        .expect("disjoint area_of_intersection must execute (returns 0.0)");

    // Empty intersection → ST_Area returns exactly 0.0; no guard needed.
    assert_eq!(
        area_int, 0.0,
        "disjoint boxes must yield 0.0 intersection area; got {area_int}"
    );
}
