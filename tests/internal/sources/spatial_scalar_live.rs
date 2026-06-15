// Internal live-Postgres probes for scalar
// PostGIS expression wrappers (`area_of` / `area_of_intersection`).
//
// The ordinary integration file covers the typed grouped-query surface for
// `convex_hull`. These scalar probes still need a raw SQL terminal because
// Djogi has no ordinary typed scalar-expression terminal for literal spatial
// expressions.

use djogi::geo::{GeoPoint, Polygon};

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

#[djogi::djogi_test(extensions = ["postgis"])]
async fn area_of_polygon_yields_positive_meters(mut ctx: djogi::DjogiContext) {
  let center = GeoPoint::new(0.0, 0.0).unwrap();
  let box_1deg = square_polygon(center, 0.5);

  let area_m2: f64 = ctx
    .raw_scalar(
      "SELECT ST_Area($1::bytea::geography)",
      &[&box_1deg.to_ewkb_bytes()],
    )
    .await
    .expect("ST_Area must run on geography polygon");

  assert!(area_m2 > 1.0e9, "area too small: {area_m2}");
  assert!(area_m2 < 1.0e11, "area too large: {area_m2}");
}

#[djogi::djogi_test(extensions = ["postgis"])]
async fn area_of_intersection_overlapping_polygons_is_positive(mut ctx: djogi::DjogiContext) {
  let center_a = GeoPoint::new(0.0, 0.0).unwrap();
  let center_b = GeoPoint::new(0.5, 0.5).unwrap();
  let box_a = square_polygon(center_a, 0.5);
  let box_b = square_polygon(center_b, 0.5);

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
  assert!(
    pct < 1.0,
    "ratio must not exceed 1.0 for partial overlap; got {pct}"
  );
  assert!(
    pct > 0.1 && pct < 0.5,
    "ratio should be ~0.25 for half-overlap squares; got {pct}"
  );
}

#[djogi::djogi_test(extensions = ["postgis"])]
async fn area_of_intersection_disjoint_polygons_is_zero(mut ctx: djogi::DjogiContext) {
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
    .expect("disjoint area_of_intersection must execute");

  assert_eq!(
    area_int, 0.0,
    "disjoint boxes must yield 0.0 intersection area; got {area_int}"
  );
}
