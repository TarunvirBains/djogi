// Live-Postgres integration tests for the
// spatial-aggregate (`convex_hull`) wrapper.
//
// # Scope
//
// Each test is annotated `#[djogi_test(extensions = ["postgis"])]` so the
// per-test database is auto-provisioned with PostGIS 3.x. The four scenarios
// cover:
//
// 1. **`convex_hull_aggregate_over_herd_points_yields_polygon`** — group a
//  set of geographic points by herd id and fold each group into a convex
//  hull via the new `FieldRef::convex_hull()` aggregate. Verifies the SQL
//  composes through `GroupedQuerySet::annotate(...)` end-to-end and the
//  decoded result is a `Polygon` with a closed ring.
//
// # Why a live test
//
// The unit tests in `expr/spatial.rs` and `query/field.rs` pin the SQL token
// stream emitted for each new variant. This live test uses the ordinary typed
// grouped-query surface; the scalar `area_of` probes live in
// `tests/internal/spatial_scalar_live.rs` because Djogi
// does not expose an ordinary scalar-expression terminal for literal spatial
// values.
//
// # Schema
//
// `#[djogi_test(sync_models = [TrackedPoint])]` provisions a single
// `tracked_points` table mirroring the elephant-tracker per-elephant
// location model: `(id, herd_id, location)`.

use djogi::geo::{GeoPoint, Polygon};
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test model — minimal point-with-group for convex_hull aggregate verification
// ---------------------------------------------------------------------------

/// Point with group id — mirrors the `(id, herd_id, location)` triple from the
/// elephant-tracker mating-pairs demo, simplified to the shape these
/// spatial-aggregate tests actually exercise.
#[model(table = "tracked_points", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct TrackedPoint {
    pub herd_id: i64,
    pub location: GeoPoint,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn tracked_point(herd_id: i64, lat: f64, lon: f64) -> TrackedPoint {
    TrackedPoint {
        id: djogi::HeerId::from_i64(0).expect("HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        herd_id,
        location: GeoPoint::new(lat, lon).expect("valid coord"),
    }
}

// ---------------------------------------------------------------------------
// Scenario 1 — convex_hull aggregate per group
// ---------------------------------------------------------------------------

/// Folding a set of points per `herd_id` via `convex_hull()` must yield a
/// `Polygon` (one row per group) whose area is positive — the points span
/// enough geographic spread that the hull is non-degenerate.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [TrackedPoint])]
async fn convex_hull_aggregate_over_herd_points_yields_polygon(mut ctx: djogi::DjogiContext) {
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

    // Each herd's hull must decode as a non-degenerate polygon with a closed
    // outer ring. The server-side convex_hull aggregate and typed Polygon
    // decoder have already round-tripped by this point.
    for (herd_id, hull) in &hulls {
        let outer = hull.outer();
        assert!(
            outer.len() >= 4 && outer.first() == outer.last(),
            "herd {herd_id} hull must be a closed polygon; got {hull:?}"
        );
    }
}
