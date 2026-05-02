> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Spatial

The `spatial` feature ships a typed PostGIS surface — coordinate types,
shape predicates, geo-aware aggregation, and a bbox/distance toolkit —
built on the Phase 4 expression substrate.

## Scope at a glance

**Phase 6 (the coordinate core):**
- `GeoPoint` — validated WGS-84 coordinate with Haversine helper,
  `GEOGRAPHY(Point, 4326)` codec, `within_km` radius filter, and
  `order_by_distance` with a primary-key tiebreak.

**Phase 6.5 (the polish layer):**
- Non-point geometries — `LineString`, `Polygon`, `MultiPoint`,
  `MultiLineString`, `MultiPolygon` — each with a manual EWKB codec.
- Shape predicates on `FieldRef` — `contains`, `intersects`, `touches`,
  `within` — across any two compatible `GeographyValue` types.
- Bounding-box prefilter (`bounded_by`) and a first-class `distance_to`
  expression composable into `filter_expr` / `annotate` / `order_by`.
- Spatial grouping — `group_by_region` / `count_by_region`,
  `cluster_by_proximity` (DBSCAN), `bucket_by_cell` (geohash).
- `#[djogi_test(extensions = [...])]` auto-provisions PostGIS (or any
  other extension name) before each test database runs its setup.

---

## Enabling the feature

Add `spatial` to your `djogi` dependency in `Cargo.toml`:

```toml
[dependencies]
djogi = { version = "...", features = ["spatial"] }
```

**Requirements:**

- PostgreSQL 18 (the Djogi floor) with the `postgis` extension installed.
  PostGIS 3.x is the tested version.
- Install the extension once at the cluster or database level before running
  your first migration:

  ```sql
  CREATE EXTENSION IF NOT EXISTS postgis;
  ```

  If your application role does not have `CREATE EXTENSION` privileges, a
  database administrator must install it. `cargo djogi migrate` (Phase 7) will
  detect the `extension_dependency` metadata on spatial indexes and surface a
  clear error when PostGIS is absent.

---

## GeoPoint basics

`GeoPoint` is a `Copy` value type carrying `lat: f64` and `lon: f64`.

### Construction

Always use `GeoPoint::new(lat, lon)` — it validates coordinates and returns
`Result<GeoPoint, GeoError>`:

```rust
use djogi::GeoPoint;

let sfo = GeoPoint::new(37.6189, -122.3750)?;  // San Francisco airport
let jfk = GeoPoint::new(40.6413, -73.7781)?;   // JFK airport
```

Validation rules:

- `lat` must be finite and in the range `-90.0..=90.0` (inclusive).
- `lon` must be finite and in the range `-180.0..=180.0` (inclusive).
- NaN and infinite inputs fail validation.

Construction via struct literal (`GeoPoint { lat, lon }`) is technically
permitted because the fields are public, but it skips validation and relies on
PostGIS's `GEOGRAPHY` type to reject invalid coordinates at INSERT time.
Prefer `GeoPoint::new`.

### Haversine distance

`GeoPoint::distance_to` returns the great-circle distance in meters using the
Haversine formula:

```rust
let meters = sfo.distance_to(jfk);
// ~4,151,000 m (San Francisco to New York)
```

This is a pure Rust computation — no database round-trip. Use it for
client-side distance checks or validation. For server-side distance filtering
and ordering, use `within_km` and `order_by_distance` (see below).

### WKT display

`Display` emits the OGC Well-Known Text format with longitude first:

```rust
let p = GeoPoint::new(37.7749, -122.4194)?;
println!("{p}");  // POINT(-122.4194 37.7749)
```

This matches PostGIS's `ST_AsText` output for `GEOGRAPHY` points.

---

## Declaring a model with a GeoPoint field

```rust
use djogi::prelude::*;

#[model(table = "places")]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: GeoPoint,
}
```

The macro emits:

- `FieldSqlType::Geography { srid: 4326 }` for the `location` field in the
  `ModelDescriptor`.
- A GiST `IndexSpec` for the column, with `requires_out_of_transaction = true`
  and `extension_dependency = Some("postgis")`.

`GeoPoint` does not implement `Default`, so the macro skips the blanket
`Default` derivation for `Place`. Struct-update syntax is unavailable on
models with `GeoPoint` fields.

---

## Querying — `within_km`

Filter rows within a given radius of a center point:

```rust
use djogi::prelude::*;

let center = GeoPoint::new(37.7749, -122.4194)?;

let nearby = Place::objects()
    .filter(|p| p.location().within_km(center, 10.0))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT id, created_at, updated_at, name, location
> FROM places
> WHERE ST_DWithin(location, ST_Point($1, $2)::geography, $3)
> ```
>
> `$1` = longitude, `$2` = latitude, `$3` = radius in meters (`km * 1000.0`).
> All three values are bound as parameters — no string interpolation of
> user-supplied coordinates.

The radius is converted from kilometers to meters before binding so the
parameter type matches `ST_DWithin`'s `GEOGRAPHY` distance-in-meters signature.

`within_km` returns a `Condition::Expr(Expr<bool>)` — the same IR node type
the Phase 4 expression substrate uses for all typed predicates. It composes
with `.and_with` / `.or_with` and with any other `Condition` in a filter
closure:

```rust
let results = Place::objects()
    .filter(|p| {
        p.location().within_km(center, 10.0)
            .and_with(p.name().contains("airport"))
    })
    .fetch_all(&mut ctx)
    .await?;
```

---

## Querying — `order_by_distance`

Order rows by ascending distance from a center point:

```rust
let center = GeoPoint::new(37.7749, -122.4194)?;

let by_distance = Place::objects()
    .order_by(|p| p.location().order_by_distance(center))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT id, created_at, updated_at, name, location
> FROM places
> ORDER BY ST_Distance(location, ST_Point($1, $2)::geography) ASC, id ASC
> ```

**Determinism contract:** the primary-key column (`id`) is appended as an
unconditional ascending tiebreak. Equidistant rows are ordered by ascending
primary key — reproducible across repeated executions and safe for keyset
pagination. Callers who chain additional `.order_by(...)` after
`order_by_distance` get their keys appended after the tiebreak:

```rust
Place::objects()
    .order_by(|p| p.location().order_by_distance(center))
    .order_by(|p| p.name().asc())   // appended after PK tiebreak
    .fetch_all(&mut ctx)
    .await?;
```

---

## Non-point geometries (Phase 6.5)

`GeoPoint` is one of six `GeographyValue` types Djogi ships. Each has a
manual EWKB codec in `djogi::geo::ewkb` and a `postgres_types::{ToSql,
FromSql}` impl against the matching PostGIS `GEOGRAPHY(<subtype>, 4326)`
column type.

| Rust type | PostGIS subtype |
|---|---|
| `GeoPoint` | `Point` |
| `LineString` | `LineString` |
| `Polygon` | `Polygon` |
| `MultiPoint` | `MultiPoint` |
| `MultiLineString` | `MultiLineString` |
| `MultiPolygon` | `MultiPolygon` |

Constructors and helpers:

```rust
use djogi::geo::{LineString, MultiPolygon, Polygon};
use djogi::GeoPoint;

let line = LineString::new(&[
    GeoPoint::new(37.7, -122.4)?,
    GeoPoint::new(37.8, -122.3)?,
])?;

let polygon = Polygon::with_ring(vec![
    GeoPoint::new(37.7, -122.4)?,
    GeoPoint::new(37.7, -122.3)?,
    GeoPoint::new(37.8, -122.3)?,
    GeoPoint::new(37.7, -122.4)?,  // OGC simple features: closing point = opening point
])?;

let coverage = MultiPolygon::new(vec![polygon.clone()])?;
```

Each constructor validates OGC Simple Features constraints (closed rings,
minimum-point counts) and returns `Result<_, GeoError>` on malformed input.

Polygon carries a single outer ring plus zero or more holes; use
`Polygon::with_rings(outer, holes)` to attach holes.

The macro's `Geography` descriptor emits `FieldSqlType::Geography {
subtype: GeographySubtype::<Variant>, srid: 4326 }` automatically for each
of these types; the same GiST `IndexSpec` is emitted (out-of-transaction,
`extension_dependency = Some("postgis")`).

---

## Shape predicates (Phase 6.5)

Any `FieldRef<M, G: GeographyValue>` exposes four shape predicates that
compose into filter closures:

```rust
use djogi::prelude::*;

let sfo_point = GeoPoint::new(37.618, -122.375)?;
let sfo_box   = some_polygon();

// ST_Contains — polygon field contains point / geometry.
let matches = Neighborhood::objects()
    .filter(|n| n.boundary().contains(&sfo_point))
    .fetch_all(&mut ctx)
    .await?;

// ST_Intersects — two geometries share at least one point.
let crossing = Route::objects()
    .filter(|r| r.path().intersects(&sfo_box))
    .fetch_all(&mut ctx)
    .await?;

// ST_Touches — share boundary points but no interior points.
let adjacent = Parcel::objects()
    .filter(|p| p.shape().touches(&left))
    .fetch_all(&mut ctx)
    .await?;

// ST_Within — field geometry is entirely inside the argument.
let inside = Parcel::objects()
    .filter(|p| p.shape().within(&region))
    .fetch_all(&mut ctx)
    .await?;
```

### Cast discipline (why `::bytea::geometry` vs `::bytea::geography`)

PostGIS 3.x splits these four functions across two type families:

- **`ST_Intersects`** has a native `geography` overload. Djogi emits:
  ```sql
  ST_Intersects(<col>, $1::bytea::geography)
  ```
- **`ST_Contains` / `ST_Touches` / `ST_Within`** are *geometry-only* — the
  geography overloads do not exist in PostGIS 3.x. Djogi casts both sides:
  ```sql
  ST_Contains(<col>::geometry, $1::bytea::geometry)
  ```

The `$n::bytea::<type>` double-cast is required because `Vec<u8>: ToSql`
binds as `bytea`. Without the `bytea` stage, `tokio_postgres` prepares
`$n` as the outer type (`geography` or `geometry`) and rejects the
`Vec<u8>` bind as a type mismatch.

The argument to each predicate can be any `GeographyValue` — they compose
across geometry types (`contains(&point)`, `intersects(&polygon)`,
`touches(&other_polygon)`, `within(&multi_polygon)`).

---

## Bounding-box prefilter and `distance_to` (Phase 6.5)

### `bounded_by` — GiST-accelerated bbox prefilter

`.filter_expr(|f| f.col().bounded_by(min_lat, min_lon, max_lat, max_lon))`
emits a `ST_MakeEnvelope(...) && <col>` predicate. The `&&` operator is
GiST-indexable, so this prefilter reaches the index even in front of more
expensive shape predicates:

```rust
let in_box = Store::objects()
    .filter_expr(|f| f.location().bounded_by(37.0, -123.0, 38.0, -122.0))
    .order_by(|f| f.location().order_by_distance(sfo))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT t.*
> FROM stores AS t
> WHERE ST_MakeEnvelope($1, $2, $3, $4, 4326)::geography && t.location
> ORDER BY ST_Distance(t.location, ST_Point($5, $6)::geography) ASC, t.id ASC
> ```

The Rust API keeps `(lat, lon)` ordering to match the `GeoPoint`
convention; the emitter reorders to Postgres's `(x, y) = (lon, lat)`
convention internally.

### `distance_to` — distance as a composable expression

`FieldRef::distance_to(&center)` returns an `Expr<f64>` in meters.
`Expr<f64>` implements the full comparison surface (`lt`, `lte`, `gt`,
`gte`, `eq`), so it composes into any predicate slot:

```rust
let sfo = GeoPoint::new(37.618, -122.375)?;

let near = Store::objects()
    .filter_expr(|f| f.location().distance_to(&sfo).lt(Expr::literal(50_000.0_f64)))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT t.*
> FROM stores AS t
> WHERE ST_Distance(t.location, ST_Point($1, $2)::geography) < $3
> ```

The `Distance` expression IR node (`SpatialExpr::Distance`) is identical
to the one that backs `order_by_distance`, reused here as a first-class
expression type.

---

## Spatial aggregates

Geography fields expose typed aggregate methods that compose into the
grouped-aggregation surface (see the
[query-aggregation guide](./query-aggregation.md)). They follow the same
shape as numeric / collection aggregates — call on a `FieldRef`, get back
an `AggregateExpr<ReturnType>`.

### `convex_hull` — minimal enclosing polygon

`FieldRef<M, GeoPoint>::convex_hull()` returns the convex hull of every
point in the group as `AggregateExpr<Polygon>`. The fused emission is
`ST_ConvexHull(ST_Collect(<col>))` with an outer geography cast:

```rust
use djogi::prelude::*;

// Hull around every sighting in each herd's territory.
let hulls: Vec<(HeerId, Polygon)> = Sighting::objects()
    .group_by(|f| f.herd_id())
    .annotate(|f| f.location().convex_hull())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT t.herd_id, ST_ConvexHull(ST_Collect(t.location::geometry))::geography AS __djogi_agg_0
> FROM sightings AS t
> GROUP BY t.herd_id
> ```

The `::geometry` inside `ST_Collect` matches PostGIS's input-type
expectation; the outer `::geography` cast keeps the round-trip on the
geography substrate so result decoding lands in `Polygon` cleanly.

`convex_hull` is also available on `FieldRef<M, Polygon>` (and the other
non-point geographies) — collecting polygons and hulling the union is
useful when you want a coarse outline around a set of regions.

### Composition with grouping

Spatial aggregates compose with every grouping mode — plain `group_by`,
`group_by_region`, `cluster_by_proximity`, `bucket_by_cell`. The
`cluster_by_proximity` examples in the next section show the pattern.

### Other PostGIS aggregates (Cluster E)

Cluster E (#88) extended the typed PostGIS aggregate surface beyond
`convex_hull`. Each method returns `AggregateExpr<ReturnType>` and
composes with `.distinct()` / `.filter()` / `.over(...)` /
`.order_by(...)` modifiers identically to numeric / collection
aggregates.

| Method | Signature | Emission | Receiver |
|---|---|---|---|
| `centroid()` | `FieldRef<M, GeoPoint> -> AggregateExpr<GeoPoint>` | `ST_Centroid(ST_Collect(<col>::geometry))::geography` | GeoPoint |
| `collect()` | `FieldRef<M, GeoPoint> -> AggregateExpr<MultiPoint>` | `ST_Collect(<col>::geometry)::geography` | GeoPoint |
| `union()` | `FieldRef<M, Polygon> -> AggregateExpr<MultiPolygon>` | `ST_Union(<col>::geometry)::geography` | Polygon, MultiPolygon |
| `extent()` | any geography → `AggregateExpr<Polygon>` | `ST_Extent(<col>::geometry)::geometry::geography` | every `GeographyValue` field |
| `extent_3d()` | any geography → `AggregateExpr<Polygon>` | `ST_3DExtent(...)` cast chain | every `GeographyValue` field |
| `make_line()` | `FieldRef<M, GeoPoint> -> AggregateExpr<LineString>` | `ST_MakeLine(<col>::geometry)::geography` | GeoPoint |
| `line_agg()` | `FieldRef<M, LineString> -> AggregateExpr<MultiLineString>` | `ST_LineAgg(<col>::geometry)::geography` | LineString |
| `polygon_agg()` | `FieldRef<M, Polygon> -> AggregateExpr<MultiPolygon>` | `ST_Collect(<col>::geometry)::geography` (portable fallback) | Polygon |
| `cluster_intersecting()` | `FieldRef<M, Polygon> -> AggregateExpr<Vec<MultiPolygon>>` | `ST_ClusterIntersecting(<col>::geometry)::geography[]` | Polygon, MultiPolygon |
| `cluster_within(d)` | `FieldRef<M, Polygon>, distance: f64 -> AggregateExpr<Vec<MultiPolygon>>` | `ST_ClusterWithin(<col>::geometry, $1)::geography[]` | Polygon, MultiPolygon |
| `mem_union()` | `FieldRef<M, Polygon> -> AggregateExpr<MultiPolygon>` | `ST_MemUnion(<col>::geometry)::geography` | Polygon, MultiPolygon |
| `polygonize()` | `FieldRef<M, LineString> -> AggregateExpr<MultiPolygon>` | `ST_Polygonize(<col>::geometry)::geography` | LineString |

#### Per-group centroid + count + IDs

The `cluster_sightings` example demo combines DBSCAN clustering with
per-cluster centroid + count + ID rollup in one typed chain — see
`examples/elephant-tracker/src/demos/cluster_sightings.rs` for the
full retrofit.

```rust
let rows: Vec<(ClusterId, (i64, GeoPoint, Vec<HeerId>))> = Sighting::objects()
    .cluster_by_proximity(
        |f| f.location(),
        ClusterRadius::meters(50_000.0).min_points(3),
    )
    .annotate(|f| (
        f.id().count_star(),
        f.location().centroid(),
        f.id().array_agg().order_by(f.id().asc()),
    ))
    .fetch_all(&mut ctx).await?;
```

#### Vector-tile / Geobuf output

`ST_AsMVT` and `ST_AsGeobuf` are row-shape aggregates (they consume
the entire annotate tuple, not a single column). They don't fit the
column-aggregate `AggOp` surface; tracked in
[#92](https://github.com/TarunvirBains/djogi/issues/92) as Cluster F
work — same v0.1.0 timeline, separate execution unit because the IR
shape differs.

---

## Spatial grouping (Phase 6.5)

Three entry points integrate spatial reasoning with the grouped-aggregation
surface documented in the [query-aggregation guide](./query-aggregation.md).

### `group_by_region` / `count_by_region`

Point-in-polygon JOIN using PostGIS's geography-native `ST_Covers`:

```rust
use djogi::query::spatial_grouping::RegionKey;

let counts: Vec<(RegionKey<Neighborhood>, i64)> = Store::objects()
    .group_by_region(|f| f.location(), Neighborhood::objects())
    .annotate(|f| f.id().count_star())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT r.id AS rk0, COUNT(*) AS __djogi_agg_0
> FROM stores AS t
> LEFT JOIN neighborhoods AS r ON ST_Covers(r.boundary, t.location)
> GROUP BY r.id
> ```

`RegionKey<R>::region_pk` is `Option<R::Pk>`. The `LEFT JOIN` preserves
rows outside every region as an `Option::None` bucket — count them as a
first-class group.

**Why `ST_Covers`, not `ST_Contains`:** PostGIS 3.x has no
`ST_Contains(geography, geography)` overload. `ST_Covers` has one and
gives the boundary-inclusive point-in-polygon answer — a point on the
polygon's edge returns `true` under `ST_Covers(polygon, point)` but
returns `false` under `ST_Contains(polygon, point)` (`ST_Contains`
requires interior intersection). The two functions agree for points in
the polygon's interior or fully outside it; they differ only for points
exactly on the boundary. For grouping, the boundary-inclusive
interpretation is the useful one — a store on a neighborhood boundary
should still count under *some* neighborhood — so `ST_Covers` is the
correct choice beyond just the geography-overload availability. Using
it also avoids `::geometry` casts under the JOIN, which would defeat
GiST-index usage on the geography column.

### `cluster_by_proximity` — DBSCAN

```rust
use djogi::query::spatial_grouping::{ClusterId, ClusterRadius};

let clusters: Vec<(ClusterId, i64)> = Store::objects()
    .cluster_by_proximity(
        |f| f.location(),
        ClusterRadius::meters(5_000.0).min_points(3),
    )
    .annotate(|f| f.id().count_star())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT cluster_id, COUNT(*) AS __djogi_agg_0
> FROM (
>     SELECT t.*,
>            ST_ClusterDBSCAN(t.location::geometry, $1, $2) OVER () AS cluster_id
>     FROM stores AS t
> ) AS t
> GROUP BY cluster_id
> ```

`ClusterId(Option<i32>)` — `Some(n)` for a cluster label, `None` for
DBSCAN noise points.

**`ClusterRadius`** is a small builder: `meters(f64)` converts to degrees
using the equator-circumference identity `(2·π·R_earth)/360` with WGS84
`R_earth = 6,378,137 m`; `degrees(f64)` accepts the raw value; `.min_points(i32)`
is DBSCAN's population floor. The unit conversion degrades at high
latitudes — for production use, tune `min_points` to compensate.

**Why the subquery wrap:** `ST_ClusterDBSCAN(...) OVER () ... GROUP BY
cluster_id` is rejected by Postgres with `ERROR: window functions are
not allowed in GROUP BY`. Djogi materialises the window call in an inner
subquery so the outer `GROUP BY cluster_id` references a plain column.

### `bucket_by_cell` — geohash precision bucketing

```rust
use djogi::query::spatial_grouping::{GeohashKey, GeohashPrecision};

let buckets: Vec<(GeohashKey, i64)> = Store::objects()
    .bucket_by_cell(|f| f.location(), GeohashPrecision::P5)
    .annotate(|f| f.id().count_star())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT ST_GeoHash(t.location::geometry, $1) AS geohash, COUNT(*) AS __djogi_agg_0
> FROM stores AS t
> GROUP BY geohash
> ```

`GeohashPrecision::{P1 .. P12}` — each `P` step increases resolution by
roughly 4×. Approximate cell sizes at the equator: `P5` ≈ 4.9 km,
`P7` ≈ 150 m, `P9` ≈ 5 m.

`GeohashKey(Option<String>)` — the `None` bucket captures rows whose
geometry is NULL, mirroring `ClusterId`'s nullability.

### Missing-GiST guard

If any side of a spatial grouping emits against a geography column with
no declared GiST index in its `ModelDescriptor::indexes`, Djogi emits a
one-shot `tracing::warn!` on the `"djogi::spatial"` target. The guard is
a `std::sync::Once` — the warn fires at most once per process lifetime
regardless of call count.

---

## `#[djogi_test(extensions = [...])]` (Phase 6.5)

The test harness macro accepts an `extensions` array of Postgres
extension names. Each per-test database runs
`CREATE EXTENSION IF NOT EXISTS "<name>"` after the HeeRanjID install and
before your setup:

```rust
#[djogi::djogi_test(extensions = ["postgis"])]
async fn within_km_filters_correctly(mut ctx: djogi::DjogiContext) {
    // postgis is already installed in this per-test database
    ctx.raw_ddl("CREATE TABLE IF NOT EXISTS places ( … )").await?;
    // … run the test …
}
```

Extension names go through a byte-level ASCII-identifier validator — no
regex, no database round-trip for validation. Invalid names fail the
macro with a span-precise `syn::Error`; runtime invalid names surface as
`DjogiError::Db` (Postgres rejects the `CREATE EXTENSION` call).

Multiple extensions can be provisioned together:

```rust
#[djogi::djogi_test(extensions = ["postgis", "pgcrypto"])]
```

---

## SRID 4326 lock and the raw-SQL escape hatch

The typed surface is locked to `GEOGRAPHY(Point, 4326)` in Phase 6. This
covers the overwhelming majority of location-based use cases (WGS-84, the
coordinate system used by GPS and mapping APIs).

For non-4326 work — custom projections, raster columns, geometry rather than
geography — use the raw-SQL escape hatch:

```rust
// Declare a custom column type in the descriptor:
// #[field(sql_type = "GEOMETRY(Polygon, 32618)")]
// pub boundary: SomeCustomType,

// Query via raw SQL:
let rows = ctx
    .raw_query::<YourRow>(
        "SELECT id, ST_AsText(boundary) FROM parcels WHERE \
         ST_Contains(boundary, ST_Point($1, $2)::geometry)",
        &[&lon as &(dyn ToSql + Sync), &lat as &(dyn ToSql + Sync)],
    )
    .await?;
```

A future phase may generalize `GeoPoint` to `GeoPoint<const SRID: u32>` if
real adoption pressure emerges. That would be an additive, non-breaking change.

---

## Migration expectations

When Phase 7's `cargo djogi migrate` ships, it will consume the spatial
metadata on `IndexSpec` to split the GiST index into a separate
`CREATE INDEX CONCURRENTLY` step — that DDL form cannot run inside a
transaction, and PostGIS must be installed before it runs.

Until Phase 7, apply the DDL by hand in your migration files:

```sql
-- Requires the postgis extension to be installed first:
-- CREATE EXTENSION IF NOT EXISTS postgis;

ALTER TABLE places
  ADD COLUMN location GEOGRAPHY(Point, 4326) NOT NULL;

CREATE INDEX CONCURRENTLY places_location_gix
  ON places USING GIST (location);
```

The `places_location_gix` name follows the `{table}_{column}_gix` convention
that the macro emits into `IndexSpec.name`.

### Index metadata

The spatial GiST index emitted by `#[derive(Model)]` sets:

- `requires_out_of_transaction = true` — Phase 7 places this index into a
  `CREATE INDEX CONCURRENTLY` step that runs outside any transaction.
- `extension_dependency = Some("postgis")` — Phase 7 verifies the extension
  is installed before emitting the index DDL.

These fields live on `IndexSpec` in `ModelDescriptor::indexes`. The
`MigrationShape` contract helper (used in tests) validates that the descriptor
encodes this policy correctly.

---

## Testing spatial code

Use `#[djogi::djogi_test]` and provision PostGIS + schema inline at the start
of each test via `ctx.raw_ddl(...)`. The `CREATE EXTENSION IF NOT EXISTS
postgis` guard is idempotent — safe to repeat in every test setup:

```rust
use djogi::prelude::*;

async fn setup(ctx: &mut DjogiContext) {
    ctx.raw_ddl("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
        .expect("install postgis");
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS places (
             id          BIGINT       PRIMARY KEY DEFAULT generate_id(),
             name        TEXT         NOT NULL,
             location    GEOGRAPHY(Point, 4326) NOT NULL,
             created_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
         );
         CREATE INDEX IF NOT EXISTS places_location_gix
             ON places USING GIST (location)",
    )
    .await
    .expect("setup places");
}

#[djogi::djogi_test]
async fn within_km_filters_correctly(mut ctx: DjogiContext) {
    setup(&mut ctx).await;

    let sfo = GeoPoint::new(37.6189, -122.3750).unwrap();
    let oak = GeoPoint::new(37.7213, -122.2207).unwrap();  // ~20 km from SFO
    let jfk = GeoPoint::new(40.6413, -73.7781).unwrap();   // ~4151 km from SFO

    // ... create, filter, assert ...
}
```

The integration tests at `tests/integration/phase6_spatial.rs` in the repo are
the canonical reference for this pattern.

---

## Deferrals

The following are candidates for a future spatial phase — not committed:

- **Arbitrary SRIDs** — `GeoPoint<const SRID: u32>` generalization. Non-4326
  work still goes through raw SQL today (see the escape-hatch section above).
- **KNN optimization** — `ST_DWithin` index hints, `ORDER BY ... <->`
  operator for index-accelerated nearest-neighbor queries.
- **Raster and topology types** — `RASTER`, PostGIS topology, `pgRouting`
  integration. Out of scope for the typed surface.
- **Automatic DDL emission for spatial tables** — Phase 7 consumes the
  `IndexSpec` metadata (`requires_out_of_transaction`,
  `extension_dependency`) to split GiST index DDL into
  `CREATE INDEX CONCURRENTLY` steps. The differ emits the split DDL
  automatically; adopters do not apply spatial index DDL by hand.

Shipped in Phase 6.5 (previously deferred):

- Non-point geometries — `LineString`, `Polygon`, `MultiPoint`,
  `MultiLineString`, `MultiPolygon`.
- Shape predicates — `ST_Contains`, `ST_Intersects`, `ST_Touches`,
  `ST_Within` typed on any `GeographyValue`.
- Bounding-box operators — `bounded_by` using `&&` under GiST.
- Spatial aggregation — `group_by_region` / `count_by_region`,
  `cluster_by_proximity`, `bucket_by_cell`.
- `#[djogi_test(extensions = [...])]` extension auto-provisioning.

---

## See also

- [Queries guide](./queries.md) — `QuerySet`, filter closures, ordering
- [Expressions guide](./expressions.md) — `Expr<T>` substrate that spatial
  predicates extend
- [Transactions guide](./transactions.md) — `atomic()` and `DjogiContext` for
  combining spatial writes with other operations
