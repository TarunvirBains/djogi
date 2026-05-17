> [Back to README](../../../ReadMe.MD)

# PostGIS function coverage (2026-05-09)

This catalog snapshot is the anchor for Djogi issue [#179](https://github.com/TarunvirBains/djogi/issues/179).

## Constructor surface status

Djogi v0.1.0 spatial alpha covers the canonical typed PostGIS shape:

- `GeoPoint`, `LineString`, `Polygon`, `MultiPoint`,
  `MultiLineString`, `MultiPolygon` construction and EWKB codecs
- Shape predicates, distance helpers, spatial expressions and grouped
  aggregate helpers already surfaced in the typed spatial API

## Deferred constructors

The following PostGIS constructors are intentionally post-v0.1.0 unless an adopter
shape escalates them:

- `ST_TileEnvelope` — Web Mercator tile envelopes; escalates into Cluster 4C
  only if MVT/Geobuf row-shape work in [#92](https://github.com/TarunvirBains/djogi/issues/92)
  makes it mandatory.
- `ST_HexagonGrid`, `ST_SquareGrid`
- `ST_Letters`
- `ST_MakePointM`
- `ST_MakeValid`, `ST_IsValidDetail`, `ST_IsValidReason`
- Longer-tail clustering (`ST_ClusterDBSCAN`, `ST_ClusterKMeans`, `ST_ClusterWithin`)
- Coverage (`ST_CoverageUnion`, `ST_CoverageSimplify`, `ST_CoverageClean`)
- Trajectory (`ST_IsValidTrajectory`, `ST_ClosestPointOfApproach`, `ST_CPAWithin`)
- I/O (`ST_AsFlatGeobuf`, `ST_AsMARC21`, `ST_AsTWKB`, GeoHash variants,
  Encoded Polyline, Geobuf)

## Policy

- v0.1.0 spatial alpha remains intentionally conservative and does not add typed wrappers
  for these constructors today.
- Adopters can continue using raw SQL via `ctx.raw_query(...)` and bypass annotations
  as an interim escape hatch.
