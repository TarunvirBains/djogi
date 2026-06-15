// Live-Postgres integration tests for the spatial-polish
// surface ( / / / ).
//
// # Scope
//
// Each test is annotated `#[djogi_test(extensions = ["postgis"])]` so the
// per-test database is auto-provisioned with PostGIS 3.x. All eleven
// scenarios are live under .5 after the four emitter fixes described
// below.
//
// ## Scenarios
//
// 1. **`contains_point_in_polygon_matches`** — `FieldRef::contains`
//  selects only the neighborhood polygon that contains the query point.
// 2. **`intersects_linestring_polygon`** — `FieldRef::intersects`
//  selects only the route that crosses the query polygon.
// 3. **`contains_point_in_multipolygon`** — `FieldRef::contains` on a
//  `MultiPolygon` finds a point that lives in one of its member polygons.
// 4. **`touches_adjacent_polygons`** — `FieldRef::touches` selects the
//  polygon that shares an edge with the query polygon but does not
//  overlap it.
// 5. **`bounded_by_with_order_by_distance_returns_expected_rows`** — bbox
//  prefilter composed with `order_by_distance`.
// 6. **`distance_to_in_filter_expr`** — `FieldRef::distance_to` composed
//  into `filter_expr` as a `lt` predicate against 50 km from SFO.
// 7. **`group_by_region_counts_stores_per_neighborhood`** — per-region
//  counts including the `None` bucket for stores outside every region.
// 8. **`count_by_region_matches_group_by_region`** — the scalar-count
//  sugar matches its `.annotate(|f| f.id().count_star())` equivalent.
// 9. **`cluster_by_proximity_dbscan_three_clusters_plus_noise`** — DBSCAN
//  over 3 tight clusters + 1 outlier yields exactly 3 non-null cluster
//  ids and one noise bucket.
// 10. **`bucket_by_cell_tight_cluster_single_bucket`** — geohash
//   bucketing at `P5` collapses 5 tightly-clustered points into one cell.
//
// # .5 emitter fixes (landed before these tests ran green)
//
// The initial run surfaced four pre-existing emitter defects; all four
// were fixed in the .5 follow-up commit so every scenario above now
// runs end-to-end. The defects and their fixes:
//
// - ** `$1::geography` bind mismatch** → `$n::bytea::geography` double
//  cast so bound EWKB bytes are prepared as `bytea` and Postgres casts to
//  `geography` at query time.
// - ** `ST_Contains` / `ST_Touches` / `ST_Within` wrong argument type**
//  → `emit_binary_predicate` now casts both the column and the bind to
//  `::geometry` for these three functions, keeping `::geography` only for
//  `ST_Intersects` (which has a native geography overload).
// - ** `ST_Contains(geography, geography)` in the JOIN** →
//  `build_spatial_join_grouped_select` now emits `ST_Covers(...)` instead,
//  which has a native `geography` overload and identical semantics for
//  the point-in-polygon use case.
// - ** window-function in GROUP BY** → `build_cluster_grouped_select`
//  now wraps the `ST_ClusterDBSCAN(...) OVER ()` call in an inner subquery
//  so the outer `GROUP BY cluster_id` references a materialised column.
//
// # Infrastructure notes
//
// - `sync_models` creates every table used by this file from the model
//  descriptors. PostGIS provisioning still flows through
//  `#[djogi_test(extensions = ["postgis"])]`.
// - The missing-GiST warn test uses a stack-allocated custom
//  `tracing::Subscriber` scoped via `set_default` — matching the pattern
//  established in `queryset.rs`'s once-warn unit test.

use djogi::geo::{GeoPoint, LineString, MultiPolygon, Polygon};
use djogi::prelude::*;
use djogi::query::spatial_grouping::{
    ClusterId, ClusterRadius, GeohashKey, GeohashPrecision, RegionKey,
};

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

/// Store with a single-point `location` — used as the "data" side in all
/// group-by-region / cluster / bucket tests.
// default flip — every model in this file is pinned to
// ascending HeerId so the explicit `djogi::HeerId::from_i64(0)` sentinels
// in the seed helpers and per-test constructions stay type-compatible
// with the injected `id` field.
#[model(table = "stores_polish", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Store {
    pub name: String,
    pub location: GeoPoint,
}

/// Neighborhood polygon — the "region" side for `group_by_region` tests.
#[model(table = "neighborhoods_polish", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Neighborhood {
    pub name: String,
    pub boundary: Polygon,
}

/// Route linestring — exercises the linestring–polygon `intersects` test.
#[model(table = "routes_polish", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Route {
    pub name: String,
    pub path: LineString,
}

/// Coverage MultiPolygon — exercises the MultiPolygon containment test.
#[model(table = "coverage_polish", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Coverage {
    pub name: String,
    pub area: MultiPolygon,
}

/// Parcel Polygon used as the "touches" adjacency fixture.
#[model(table = "parcels_polish", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Parcel {
    pub name: String,
    pub shape: Polygon,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn store(name: &str, lat: f64, lon: f64) -> Store {
    Store {
        id: djogi::HeerId::from_i64(0).expect("HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: name.to_string(),
        location: GeoPoint::new(lat, lon).expect("valid coord"),
    }
}

fn neighborhood(name: &str, boundary: Polygon) -> Neighborhood {
    Neighborhood {
        id: djogi::HeerId::from_i64(0).expect("HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: name.to_string(),
        boundary,
    }
}

/// Build a square polygon (lat/lon box) centered at `center` with side length
/// `half_side` degrees. Rings are closed — first point equals last point as
/// required by OGC simple features.
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
// Scenario 1 — contains (point-in-polygon)
// ---------------------------------------------------------------------------

/// A neighborhood polygon must be selectable by
/// `.filter(|n| n.boundary().contains(&point))` when `point` falls inside.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Neighborhood])]
async fn contains_point_in_polygon_matches(mut ctx: djogi::DjogiContext) {
    // Square polygon centered at SFO (37.618, -122.375) with ~0.1° sides
    // (~11 km per side).
    let sfo = GeoPoint::new(37.618, -122.375).unwrap();
    let sfo_box = square_polygon(sfo, 0.1);
    Neighborhood::create(&mut ctx, neighborhood("sfo_box", sfo_box.clone()))
        .await
        .expect("create neighborhood");

    // Create a second polygon elsewhere so a naive unfiltered query would
    // return both rows.
    let jfk = GeoPoint::new(40.6413, -73.7781).unwrap();
    let jfk_box = square_polygon(jfk, 0.1);
    Neighborhood::create(&mut ctx, neighborhood("jfk_box", jfk_box))
        .await
        .expect("create JFK neighborhood");

    // A point inside the SFO box.
    // PR3: PostGIS `ST_Contains` is database-locale; route via
    // `explicit_pg_predicate()`.
    let inside = GeoPoint::new(37.620, -122.370).unwrap();
    let matches = Neighborhood::objects()
        .filter(|n| n.boundary().explicit_pg_predicate().contains(&inside))
        .fetch_all(&mut ctx)
        .await
        .expect("contains query must succeed");

    assert_eq!(
        matches.len(),
        1,
        "exactly one polygon must contain the point"
    );
    assert_eq!(matches[0].name, "sfo_box");
}

// ---------------------------------------------------------------------------
// Scenario 2 — intersects (linestring–polygon)
// ---------------------------------------------------------------------------

/// A linestring that crosses a polygon's interior must satisfy
/// `FieldRef::intersects(&polygon)`.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Route])]
async fn intersects_linestring_polygon(mut ctx: djogi::DjogiContext) {
    // A 2-point linestring crossing the SFO box east–west.
    let line = LineString::new(&[
        GeoPoint::new(37.618, -122.5).unwrap(), // west of SFO
        GeoPoint::new(37.618, -122.2).unwrap(), // east of SFO
    ])
    .unwrap();
    let crossing = Route {
        id: djogi::HeerId::from_i64(0).unwrap(),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        name: "west_east".to_string(),
        path: line,
    };
    Route::create(&mut ctx, crossing)
        .await
        .expect("create route");

    // A linestring that does not cross the SFO box — positioned far north.
    let far = LineString::new(&[
        GeoPoint::new(45.0, -122.5).unwrap(),
        GeoPoint::new(45.0, -122.2).unwrap(),
    ])
    .unwrap();
    Route::create(
        &mut ctx,
        Route {
            id: djogi::HeerId::from_i64(0).unwrap(),
            created_at: djogi::DateTime::UNIX_EPOCH,
            updated_at: djogi::DateTime::UNIX_EPOCH,
            name: "far_north".to_string(),
            path: far,
        },
    )
    .await
    .expect("create far route");

    let sfo_box = square_polygon(GeoPoint::new(37.618, -122.375).unwrap(), 0.1);

    // PR3: PostGIS `ST_Intersects` is database-locale; route via
    // `explicit_pg_predicate()`.
    let hits = Route::objects()
        .filter(|r| r.path().explicit_pg_predicate().intersects(&sfo_box))
        .fetch_all(&mut ctx)
        .await
        .expect("intersects query must succeed");

    assert_eq!(hits.len(), 1, "exactly one route crosses the polygon");
    assert_eq!(hits[0].name, "west_east");
}

// ---------------------------------------------------------------------------
// Scenario 3 — MultiPolygon containment
// ---------------------------------------------------------------------------

/// A `MultiPolygon` must be selectable by `.contains(&point)` when one of its
/// member polygons contains the point.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Coverage])]
async fn contains_point_in_multipolygon(mut ctx: djogi::DjogiContext) {
    let box_a = square_polygon(GeoPoint::new(37.618, -122.375).unwrap(), 0.1);
    let box_b = square_polygon(GeoPoint::new(40.6413, -73.7781).unwrap(), 0.1);
    let two_boxes = MultiPolygon::new(vec![box_a, box_b]).expect("valid two-polygon MultiPolygon");

    Coverage::create(
        &mut ctx,
        Coverage {
            id: djogi::HeerId::from_i64(0).unwrap(),
            created_at: djogi::DateTime::UNIX_EPOCH,
            updated_at: djogi::DateTime::UNIX_EPOCH,
            name: "two_cities".to_string(),
            area: two_boxes,
        },
    )
    .await
    .expect("create coverage");

    // Point inside box_b (JFK neighborhood). PR3: PostGIS `ST_Contains`
    // is a database-locale shape predicate, so the closure routes through
    // `explicit_pg_predicate()` to reach it.
    let jfk_point = GeoPoint::new(40.64, -73.78).unwrap();
    let hits = Coverage::objects()
        .filter(|c| c.area().explicit_pg_predicate().contains(&jfk_point))
        .fetch_all(&mut ctx)
        .await
        .expect("multipolygon contains query must succeed");

    assert_eq!(hits.len(), 1, "the MultiPolygon must contain the JFK point");

    // Point outside both member polygons (mid-Atlantic).
    let at_sea = GeoPoint::new(30.0, -50.0).unwrap();
    let misses = Coverage::objects()
        .filter(|c| c.area().explicit_pg_predicate().contains(&at_sea))
        .fetch_all(&mut ctx)
        .await
        .expect("multipolygon contains query must succeed");
    assert!(misses.is_empty(), "point at sea must not be contained");
}

// ---------------------------------------------------------------------------
// Scenario 4 — touches
// ---------------------------------------------------------------------------

/// Two polygons sharing a single edge (no interior overlap) must satisfy
/// `ST_Touches`. The test creates two squares that share their vertical
/// border, then queries one against the other.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Parcel])]
async fn touches_adjacent_polygons(mut ctx: djogi::DjogiContext) {
    // Left square: lat 0..1, lon 0..1.
    let left_pts = vec![
        GeoPoint::new(0.0, 0.0).unwrap(),
        GeoPoint::new(0.0, 1.0).unwrap(),
        GeoPoint::new(1.0, 1.0).unwrap(),
        GeoPoint::new(1.0, 0.0).unwrap(),
        GeoPoint::new(0.0, 0.0).unwrap(),
    ];
    let left = Polygon::with_ring(left_pts).unwrap();

    // Right square: lat 0..1, lon 1..2 — shares the east edge of `left`.
    let right_pts = vec![
        GeoPoint::new(0.0, 1.0).unwrap(),
        GeoPoint::new(0.0, 2.0).unwrap(),
        GeoPoint::new(1.0, 2.0).unwrap(),
        GeoPoint::new(1.0, 1.0).unwrap(),
        GeoPoint::new(0.0, 1.0).unwrap(),
    ];
    let right = Polygon::with_ring(right_pts).unwrap();

    // A disjoint square far away.
    let far_pts = vec![
        GeoPoint::new(10.0, 10.0).unwrap(),
        GeoPoint::new(10.0, 11.0).unwrap(),
        GeoPoint::new(11.0, 11.0).unwrap(),
        GeoPoint::new(11.0, 10.0).unwrap(),
        GeoPoint::new(10.0, 10.0).unwrap(),
    ];
    let far = Polygon::with_ring(far_pts).unwrap();

    for (name, shape) in [("left", left.clone()), ("right", right), ("far", far)] {
        Parcel::create(
            &mut ctx,
            Parcel {
                id: djogi::HeerId::from_i64(0).unwrap(),
                created_at: djogi::DateTime::UNIX_EPOCH,
                updated_at: djogi::DateTime::UNIX_EPOCH,
                name: name.to_string(),
                shape,
            },
        )
        .await
        .expect("create parcel");
    }

    // Query: find parcels that touch `left`. Expect exactly `right`.
    // `left` itself overlaps (shares full interior) and is excluded by
    // `ST_Touches`, which requires shared boundaries but disjoint interiors.
    // PR3: `ST_Touches` is PostGIS-specific; route through
    // `explicit_pg_predicate()` from the post-flip `DjogiField` surface.
    let touching = Parcel::objects()
        .filter(|p| p.shape().explicit_pg_predicate().touches(&left))
        .fetch_all(&mut ctx)
        .await
        .expect("touches query must succeed");

    let names: Vec<&str> = touching.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["right"],
        "only the right parcel shares an edge with left; got {:?}",
        names
    );
}

// ---------------------------------------------------------------------------
// Scenario 5 — bbox prefilter + order_by_distance
// ---------------------------------------------------------------------------

/// Confirms two things:
///
/// 1. A `.filter_expr(|f| f.location().bounded_by(...))` predicate composed
///  with `.order_by(|f| f.location().order_by_distance(center))` returns the
///  expected rows (every bay-area store inside the bbox; every remote store
///  excluded).
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Store])]
async fn bounded_by_with_order_by_distance_returns_expected_rows(mut ctx: djogi::DjogiContext) {
    // Seed 20 stores: 10 inside the SF Bay box, 10 scattered worldwide.
    let sfo = GeoPoint::new(37.618, -122.375).unwrap();
    for i in 0..10 {
        let lat = 37.6 + 0.01 * (i as f64);
        let lon = -122.4 + 0.01 * (i as f64);
        Store::create(&mut ctx, store(&format!("bay_{i}"), lat, lon))
            .await
            .expect("create bay store");
    }
    for (i, (lat, lon)) in [
        (40.6413, -73.7781),
        (34.0522, -118.2437),
        (41.8781, -87.6298),
        (29.7604, -95.3698),
        (39.9526, -75.1652),
        (33.4484, -112.0740),
        (32.7157, -117.1611),
        (30.2672, -97.7431),
        (47.6062, -122.3321),
        (42.3601, -71.0589),
    ]
    .iter()
    .enumerate()
    {
        Store::create(&mut ctx, store(&format!("remote_{i}"), *lat, *lon))
            .await
            .expect("create remote store");
    }

    // Filter to a ~1° box around SFO, order by distance from SFO.
    // PR3: `bounded_by` is a spatial expression-only predicate, so it
    // routes through `explicit_pg_predicate()`. `order_by_distance`
    // remains direct on `DjogiField` because spatial ordering is not a
    // cache predicate boundary.
    let in_box = Store::objects()
        .filter_expr(|f| {
            f.location()
                .explicit_pg_predicate()
                .bounded_by(37.0, -123.0, 38.0, -122.0)
        })
        .order_by(|f| f.location().order_by_distance(sfo))
        .fetch_all(&mut ctx)
        .await
        .expect("bbox + order_by_distance must succeed");

    // All 10 bay stores are inside the box. Remote stores are outside.
    // Seattle at 47.6 is outside lat range; LA at 34 is outside; all others too.
    assert_eq!(
        in_box.len(),
        10,
        "expected 10 bay-area stores inside the bbox, got {}; names: {:?}",
        in_box.len(),
        in_box.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Scenario 6 — distance_to as filter predicate
// ---------------------------------------------------------------------------

/// `.filter_expr(|f| f.location().distance_to(&center).lt(...))` composes the
/// `ST_Distance` expression into a boolean predicate — exercises the
/// expression-IR path for `Distance` and the `Expr<f64>::lt(literal)`
/// comparison.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Store])]
async fn distance_to_in_filter_expr(mut ctx: djogi::DjogiContext) {
    let sfo = GeoPoint::new(37.618, -122.375).unwrap();
    Store::create(&mut ctx, store("sfo", 37.618, -122.375))
        .await
        .unwrap();
    Store::create(&mut ctx, store("oak", 37.7213, -122.2207))
        .await
        .unwrap();
    Store::create(&mut ctx, store("jfk", 40.6413, -73.7781))
        .await
        .unwrap();

    // `distance_to` returns meters. 50 000 m = 50 km — SFO + OAK qualify, JFK
    // does not. PR3: `distance_to` is on `ExplicitPgPredicateField` because
    // PostGIS distance expressions cannot evaluate in Punnu; route via
    // `explicit_pg_predicate()` and lower the boolean comparison to SQL.
    let near: Vec<Store> = Store::objects()
        .filter_expr(|f| {
            f.location()
                .explicit_pg_predicate()
                .distance_to(&sfo)
                .lt(Expr::literal(50_000.0_f64))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("distance_to filter_expr must succeed");

    let names: Vec<&str> = near.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names.len(),
        2,
        "expected 2 stores within 50 km of SFO; got {:?}",
        names
    );
    assert!(names.contains(&"sfo"));
    assert!(names.contains(&"oak"));
    assert!(!names.contains(&"jfk"));
}

// ---------------------------------------------------------------------------
// Scenario 7 — group_by_region
// ---------------------------------------------------------------------------

/// Seeds 3 non-overlapping neighborhood polygons plus 10 stores — 3 in the
/// first region, 4 in the second, 2 in the third, and 1 outside all regions
/// — and asserts the counts per `RegionKey`, including the `None` bucket.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Store, Neighborhood])]
async fn group_by_region_counts_stores_per_neighborhood(mut ctx: djogi::DjogiContext) {
    // Three disjoint polygons. Using lat/lon degrees, ~0.1° half-side = ~11 km.
    let r1_center = GeoPoint::new(37.7, -122.4).unwrap();
    let r2_center = GeoPoint::new(40.6, -73.8).unwrap();
    let r3_center = GeoPoint::new(34.0, -118.2).unwrap();

    let n1 = Neighborhood::create(&mut ctx, neighborhood("r1", square_polygon(r1_center, 0.1)))
        .await
        .unwrap();
    let n2 = Neighborhood::create(&mut ctx, neighborhood("r2", square_polygon(r2_center, 0.1)))
        .await
        .unwrap();
    let n3 = Neighborhood::create(&mut ctx, neighborhood("r3", square_polygon(r3_center, 0.1)))
        .await
        .unwrap();

    // 3 stores in r1, 4 in r2, 2 in r3, 1 outside all regions.
    let mut points: Vec<(String, f64, f64)> = Vec::new();
    for i in 0..3 {
        points.push((
            format!("r1_store_{i}"),
            37.7 + 0.01 * i as f64,
            -122.4 + 0.01 * i as f64,
        ));
    }
    for i in 0..4 {
        points.push((
            format!("r2_store_{i}"),
            40.6 + 0.01 * i as f64,
            -73.8 + 0.01 * i as f64,
        ));
    }
    for i in 0..2 {
        points.push((
            format!("r3_store_{i}"),
            34.0 + 0.01 * i as f64,
            -118.2 + 0.01 * i as f64,
        ));
    }
    // An outlier at (50.0, 0.0) — nowhere near any region.
    points.push(("outlier".to_string(), 50.0, 0.0));

    for (n, lat, lon) in points {
        Store::create(&mut ctx, store(&n, lat, lon)).await.unwrap();
    }

    let rows: Vec<(RegionKey<Neighborhood>, i64)> = Store::objects()
        .group_by_region(|f| f.location(), Neighborhood::objects())
        .annotate(|f| f.id().count_star())
        .fetch_all(&mut ctx)
        .await
        .expect("group_by_region must succeed");

    // Build a map keyed by Option<region_pk>.
    let mut counts: std::collections::BTreeMap<Option<djogi::HeerId>, i64> = Default::default();
    for (key, count) in rows {
        counts.insert(key.region_pk, count);
    }

    assert_eq!(counts.get(&Some(n1.id)).copied(), Some(3), "r1 count");
    assert_eq!(counts.get(&Some(n2.id)).copied(), Some(4), "r2 count");
    assert_eq!(counts.get(&Some(n3.id)).copied(), Some(2), "r3 count");
    // The `None` bucket must capture the 1 outlier store.
    assert_eq!(counts.get(&None).copied(), Some(1), "outside-region bucket");
}

// ---------------------------------------------------------------------------
// Scenario 8 — count_by_region
// ---------------------------------------------------------------------------

/// Same dataset as scenario 7; asserts the scalar-count sugar matches
/// `group_by_region(...).annotate(|f| f.id.count_star())`.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Store, Neighborhood])]
async fn count_by_region_matches_group_by_region(mut ctx: djogi::DjogiContext) {
    // Two disjoint regions + 5 stores; 2 in r1, 3 in r2.
    let r1 = Neighborhood::create(
        &mut ctx,
        neighborhood(
            "r1",
            square_polygon(GeoPoint::new(37.7, -122.4).unwrap(), 0.1),
        ),
    )
    .await
    .unwrap();
    let r2 = Neighborhood::create(
        &mut ctx,
        neighborhood(
            "r2",
            square_polygon(GeoPoint::new(40.6, -73.8).unwrap(), 0.1),
        ),
    )
    .await
    .unwrap();

    for i in 0..2 {
        Store::create(
            &mut ctx,
            store(&format!("r1_{i}"), 37.7 + 0.01 * i as f64, -122.4),
        )
        .await
        .unwrap();
    }
    for i in 0..3 {
        Store::create(
            &mut ctx,
            store(&format!("r2_{i}"), 40.6 + 0.01 * i as f64, -73.8),
        )
        .await
        .unwrap();
    }

    let rows: Vec<(RegionKey<Neighborhood>, i64)> = Store::objects()
        .count_by_region(|f| f.location(), Neighborhood::objects())
        .fetch_all(&mut ctx)
        .await
        .expect("count_by_region must succeed");

    let mut counts: std::collections::BTreeMap<Option<djogi::HeerId>, i64> = Default::default();
    for (k, c) in rows {
        counts.insert(k.region_pk, c);
    }
    assert_eq!(counts.get(&Some(r1.id)).copied(), Some(2));
    assert_eq!(counts.get(&Some(r2.id)).copied(), Some(3));
}

// ---------------------------------------------------------------------------
// Scenario 9 — cluster_by_proximity (DBSCAN)
// ---------------------------------------------------------------------------

/// Seeds 16 stores: 5 tightly clustered near (-122.4, 37.8), 5 near
/// (-122.3, 37.8), 5 near (-122.2, 37.8), and 1 isolated outlier at
/// (-125.0, 40.0). With `min_points(3)` and a small radius, DBSCAN must
/// produce exactly 3 non-null cluster ids and one `ClusterId(None)` for the
/// outlier.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Store])]
async fn cluster_by_proximity_dbscan_three_clusters_plus_noise(mut ctx: djogi::DjogiContext) {
    // Three tight clusters, 5 points each, arranged along the same latitude
    // so the inter-cluster distance is ~0.1° and the intra-cluster jitter is
    // <0.001°. With `ClusterRadius::meters(5_000.0).min_points(3)` (eps ≈
    // 0.045°), each cluster's core is well within the radius and inter-cluster
    // distance is comfortably beyond it.
    let centers = [(-122.4_f64, 37.8), (-122.3, 37.8), (-122.2, 37.8)];
    for (i, (lon, lat)) in centers.iter().enumerate() {
        for k in 0..5 {
            let jitter = 0.0001 * (k as f64);
            Store::create(
                &mut ctx,
                store(&format!("c{i}_p{k}"), lat + jitter, lon + jitter),
            )
            .await
            .unwrap();
        }
    }
    // Outlier far from all clusters.
    Store::create(&mut ctx, store("outlier", 40.0, -125.0))
        .await
        .unwrap();

    let rows: Vec<(ClusterId, i64)> = Store::objects()
        .cluster_by_proximity(
            |f| f.location(),
            ClusterRadius::meters(5_000.0).min_points(3),
        )
        .annotate(|f| f.id().count_star())
        .fetch_all(&mut ctx)
        .await
        .expect("cluster_by_proximity must succeed");

    // Count distinct non-null cluster ids and the noise bucket.
    let mut non_null = std::collections::BTreeSet::new();
    let mut noise_count = 0i64;
    let mut total = 0i64;
    for (ClusterId(opt), c) in &rows {
        total += c;
        match opt {
            Some(id) => {
                non_null.insert(*id);
            }
            None => noise_count += c,
        }
    }

    assert_eq!(total, 16, "expected 16 total points; got {total}");
    assert_eq!(
        non_null.len(),
        3,
        "DBSCAN must produce exactly 3 non-null clusters; got {non_null:?}"
    );
    assert_eq!(
        noise_count, 1,
        "the isolated point must fall into the noise bucket"
    );
}

// ---------------------------------------------------------------------------
// Scenario 10 — bucket_by_cell (geohash)
// ---------------------------------------------------------------------------

/// Five points within a ~1 km neighborhood (well inside a single P5 geohash
/// cell of ~4.9 km × 4.9 km) must all land in the same bucket. A single
/// point in a far-away region lands in its own bucket.
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Store])]
async fn bucket_by_cell_tight_cluster_single_bucket(mut ctx: djogi::DjogiContext) {
    // Five points clustered inside one square km near SFO (37.618, -122.375).
    // The jitter magnitude (0.0001° ≈ 11 m) is well inside a P5 cell.
    for k in 0..5 {
        let jitter = 0.0001 * (k as f64);
        Store::create(
            &mut ctx,
            store(&format!("tight_{k}"), 37.618 + jitter, -122.375 + jitter),
        )
        .await
        .unwrap();
    }
    // One distant point — New York.
    Store::create(&mut ctx, store("nyc", 40.6413, -73.7781))
        .await
        .unwrap();

    let rows: Vec<(GeohashKey, i64)> = Store::objects()
        .bucket_by_cell(|f| f.location(), GeohashPrecision::P5)
        .annotate(|f| f.id().count_star())
        .fetch_all(&mut ctx)
        .await
        .expect("bucket_by_cell must succeed");

    // Map each `GeohashKey` to its count.
    let mut counts: std::collections::BTreeMap<Option<String>, i64> = Default::default();
    for (GeohashKey(opt), c) in rows {
        counts.insert(opt, c);
    }

    // There must be exactly 2 non-null buckets — the SFO cluster cell and the
    // NYC cell.
    let non_null_counts: Vec<(String, i64)> = counts
        .iter()
        .filter_map(|(k, c)| k.as_ref().map(|key| (key.clone(), *c)))
        .collect();
    assert_eq!(
        non_null_counts.len(),
        2,
        "expected exactly 2 geohash buckets; got {:?}",
        non_null_counts
    );
    let max_count = non_null_counts.iter().map(|(_, c)| *c).max().unwrap();
    let min_count = non_null_counts.iter().map(|(_, c)| *c).min().unwrap();
    assert_eq!(max_count, 5, "the SFO cluster bucket must hold 5 stores");
    assert_eq!(min_count, 1, "the NYC bucket must hold 1 store");
}
