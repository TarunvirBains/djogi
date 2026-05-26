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
  expression composable into `filter_expr` and expression-aware ordering.
- Spatial grouping — `group_by_region` / `count_by_region`,
  `cluster_by_proximity` (DBSCAN), `bucket_by_cell` (geohash).
- `#[djogi_test(extensions = [...])]` auto-provisions PostGIS (or any
  other extension name) before each test database runs its setup.

### PostGIS constructor coverage policy (v0.1.0 anchor)

Djogi v0.1.0 spatial alpha intentionally limits typed coverage to a
canonical surface (`GeoPoint`, `LineString`, `Polygon`, `MultiPoint`,
`MultiLineString`, `MultiPolygon`) plus the shipped relationship / distance /
aggregation expressions. PostGIS constructors are intentionally delayed:

- `ST_TileEnvelope` (escalate only if Cluster 4C [#92](https://github.com/TarunvirBains/djogi/issues/92) makes it a hard requirement for MVT/Geobuf row-shape work)
- `ST_HexagonGrid`, `ST_SquareGrid`, `ST_Letters`, `ST_MakePointM`,
  `ST_MakeValid`, `ST_IsValidDetail`, `ST_IsValidReason`
- Long-tail clustering, coverage, trajectory, and I/O constructors (`ST_CoverageUnion`,
  `ST_CoverageSimplify`, `ST_CoverageClean`, `ST_IsValidTrajectory`,
  `ST_ClosestPointOfApproach`, `ST_CPAWithin`, `ST_AsFlatGeobuf`,
  `ST_AsMARC21`, `ST_AsTWKB`, GeoHash variants, encoded polyline, Geobuf)
- **K-Means clustering** — `ST_ClusterKMeans` remains deferred.

Interim escape hatch remains raw SQL via `ctx.raw_query(...)` and related
[`#[djogi::deliberately_bypass_convention_with_raw_sql]`](../spec/raw-sql-escape-hatches.md#3-bypass-attribute) annotations. See [#179](https://github.com/TarunvirBains/djogi/issues/179) and
`docs/research/postgres-coverage/2026-05-09/02-postgis-functions.md` for the
full constructor inventory.

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
  database administrator must install it. When the migration that introduces a
  spatial index is applied via `djogi::migrate::apply_plan` (the public
  library entry point; the `apply` CLI dispatcher is deferred to a Phase 7
  follow-up), the runner reads the `extension_dependency` metadata on the
  index and surfaces a clear error if PostGIS is absent.

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
the entire projected row, not a single column). They therefore live on
the terminal surface instead of the column-aggregate `AggregateExpr`
surface:

```rust
use djogi::prelude::*;

let tile: Vec<u8> = Store::objects()
    .filter(|s| s.name().icontains("airport".to_string()))
    .as_mvt_with_options(
        MvtOptions::new("stores")
            .with_geom_name("location")
            .with_feature_id_name("id"),
    )
    .fetch_one(&mut ctx)
    .await?;

let geobuf: Vec<u8> = Store::objects()
    .as_geobuf("location")
    .fetch_one(&mut ctx)
    .await?;
```

The annotated form uses the same terminal after `.annotate(...)`, so
aggregate annotations become row properties before PostGIS encodes the
result:

```rust
let tile: Vec<u8> = Store::objects()
    .annotate(|s| s.id().count_star())
    .as_mvt("stores")
    .fetch_one(&mut ctx)
    .await?;
```

Both terminals return Postgres `bytea` as `Vec<u8>`. `QuerySet::none()`
short-circuits and returns `Ok(Vec::new())` without SQL. For normal
zero-row filters, `ST_AsGeobuf` currently returns SQL `NULL`; Djogi now
maps that to `Ok(Vec::new())` so consumers can treat that as an empty
payload.

Djogi stores spatial
fields as `geography(...)`; the row-aggregate terminal casts those inner
row columns to `geometry` while preserving their column names because
PostGIS's encoders look up the geometry column by name.

Row aggregates are deliberately not `AggregateExpr`s. They do not expose
`.distinct()`, `.filter(...)`, `.over(...)`, `.order_by(...)`, or
`.within_group_order_by(...)`; those modifiers are valid only for
column-shape aggregates.

---

## Intersection expressions (Phase 8-Zero T17)

Two static constructors on `Expr` expose PostGIS's `ST_Intersection` /
`ST_Area` pair as typed expressions composable with `filter_expr` and
arithmetic operators.

> **`annotate` limitation (v0.1.0).** `QuerySet::annotate` accepts a
> closure returning an `AggregateExpr` or window-function expression — it
> does **not** accept a bare `Expr<T>` today. `Expr::intersection_of` and
> `Expr::area_of_intersection` return `Expr<Polygon>` / `Expr<f64>`, so
> they are not directly usable as `annotate` values. They compose in any
> expression context that accepts `Expr<T>` — `filter_expr`, arithmetic
> combinations with other `Expr` nodes, and comparison methods (`.lt`,
> `.gte`, etc.). Annotating by a bare expression is tracked as a future
> phase item alongside the broader `Expr<T>` annotation deferral (see
> [Computed Properties](./computed.md)).

### `Expr::intersection_of` — raw intersection geometry

```rust
use djogi::{prelude::*, geo::Polygon};

let overlap_shape: Expr<Polygon> =
    Expr::intersection_of(&hull_a, &hull_b);
```

Emitted SQL:

```sql
ST_Intersection($n::bytea::geometry, $m::bytea::geometry)::geography
```

Both inputs are cast `::bytea::geometry` (PostGIS 3.x has no `geography`
overload for `ST_Intersection`); the result is cast `::geography` so
`Polygon`'s `FromSql` codec can decode it.

> **⚠ Decode safety warning.** The expression returns `Expr<Polygon>`.
> Decoding succeeds **only** when `ST_Intersection` yields a single
> `POLYGON`. PostGIS may return a different geometry type even for
> polygonal inputs that genuinely overlap:
>
> | Case | PostGIS result | Decode outcome |
> |---|---|---|
> | Disjoint inputs | empty geometry | **decode error** |
> | Boundary-only contact | `LINESTRING` or `POINT` | **decode error** |
> | Multi-part overlap | `MULTIPOLYGON` or `GEOMETRYCOLLECTION` | **decode error** |
>
> `FieldRef::intersects(...)` guards against the disjoint case only — it
> does NOT guarantee the result will be a single `POLYGON`.
>
> **For most use cases, prefer `Expr::area_of_intersection` instead.** It
> wraps the result in `ST_Area`, always returns `f64`, and yields `0.0`
> for all non-overlapping pairs without any guard.

### `Expr::area_of_intersection` — overlap area in square meters

```rust
// Overlap percentage — safe for disjoint polygons (yields 0.0).
let pct: Expr<f64> =
    Expr::area_of_intersection(&hull_a, &hull_b) / Expr::area_of(&hull_a);
```

Emitted SQL:

```sql
ST_Area(ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography)
/ ST_Area($3::bytea::geography)
```

`$1` and `$3` both bind `hull_a`'s EWKB bytes; `$2` binds `hull_b`'s.
Each `Expr` node compiles to its own `push_bind` call, so the same
geometry value binds as a fresh parameter at each use site rather than
being referenced by a shared positional index. The repeated bind is cheap
(a `Vec<u8>` clone for each side) and keeps the SQL emitter stateless.

`Expr::area_of_intersection(a, b)` and `Expr::area_of(g)` both return
`Expr<f64>` and compose with the full arithmetic operator set (`/`, `*`,
`+`, `-`) as well as comparison methods (`.lt`, `.gte`, `.eq`, …). This
makes overlap-percentage scoring — a common territory-analysis pattern —
expressible in one typed chain:

```rust
// Is a at least 50 % covered by b?
let heavily_overlapping = Zone::objects()
    .filter_expr(|_| {
        Expr::area_of_intersection(&hull_a, &hull_b)
            .gte(Expr::area_of(&hull_a) * Expr::literal(0.5_f64))
    })
    .fetch_all(&mut ctx)
    .await?;
```

When the inputs are disjoint, `ST_Intersection` returns an empty geometry
and `ST_Area` over an empty geography returns `0.0` — no special guard is
needed.

### Choosing between the two

| Need | Use |
|---|---|
| Area overlap (ratio, threshold, scoring) | `Expr::area_of_intersection` |
| Inspect/store the raw intersection shape | `Expr::intersection_of` — only when caller guarantees a simple single-polygon result |

---

## Pair-side territory overlap

`Expr::area_of_intersection(&a, &b)` above takes two `Polygon` *values* —
EWKB blobs known at query-build time — and binds them as `bytea` literals.
This is the right shape when the application has the two geometries in
hand before issuing the query (e.g. comparing a candidate match against a
stored reference shape).

When the geometries live on *per-row* columns of a joined pair-tuple —
"for each `(L, R)` pair, what fraction of `L.territory` overlaps
`R.territory`?" — the scalar `Expr` API does not apply: the values are
not known at query-build time, they are read out of each row pair by the
SQL engine.

[`PairAreaOverlapRatio<L, R>`](https://docs.rs/djogi/latest/djogi/query/struct.PairAreaOverlapRatio.html)
fills this gap. It is the pair-tuple annotation slot that emits

```sql
COALESCE(ST_Area(ST_Intersection(l.<lcol>::geometry,
                                  r.<rcol>::geometry)::geography), 0)::float8
  / NULLIF(ST_Area(l.<lcol>::geography), 0)::float8
```

as one SELECT-list column per pair on a `JoinedQuerySet<L, R>`.

```rust
use djogi::prelude::*;
use djogi::query::PairAreaOverlapRatio;

// Per-pair territory overlap ratio in [0, 1] across every herd-pair.
let overlaps: Vec<((Herd, Herd), f64)> = Herd::objects()
    .self_pairs()
    .include_equal_pk()
    .annotate(|l, r| PairAreaOverlapRatio::new(l.territory(), r.territory()))
    .fetch_all(&mut ctx)
    .await?;
```

### What the ratio means

The denominator is always the *left* side's area. The ratio is
asymmetric: `overlap(A, B) = area(A ∩ B) / area(A)`, the fraction of
A's territory shared with B. For Jaccard-style symmetry, fetch the
inverse pair too and combine in Rust.

| Geometry case | Ratio |
|---|---|
| Fully-coincident territories (same polygon) | `1.0` |
| Disjoint territories | `0.0` |
| Partial overlap | fraction in `(0, 1)` |
| `NULL` on either side | `0.0` (NULLIF + decode-as-`Option<f64>`) |

### When the column may be `Option<Polygon>`

Both bare (`territory: Polygon`) and nullable (`territory: Option<Polygon>`)
column types are admitted by the constructor — the
[`SpatialColumnValue`](https://docs.rs/djogi/latest/djogi/geo/trait.SpatialColumnValue.html)
seal admits both. Nullable shapes are the common case in adopter schemas
where territory polygons are materialised lazily (e.g. only after a
herd has accumulated ≥ 3 sightings).

### Compose with closure-based kinship in one query

The annotation slot composes inside a pair-tuple annotation tuple
alongside [`PairClosureKinshipSum<C>`](https://docs.rs/djogi/latest/djogi/query/struct.PairClosureKinshipSum.html)
**when both slots reference the same pair-tuple shape**: same model
on both sides (`(L, R)` with the same columns), same FROM clause.
That lets adopters emit one query returning the full
`(Wright F, territory overlap, …)` tuple per pair without three separate
round-trips:

```rust
use djogi::query::{PairAreaOverlapRatio, PairClosureKinshipSum};

// One query — kinship + overlap per (left, right) pair.
let combined: Vec<((Elephant, Elephant), (f64, f64))> = Elephant::objects()
    .self_pairs()
    .left_join_closure_pair::<ElephantAncestry>()
    .annotate(|l, r| (
        PairClosureKinshipSum::<ElephantAncestry>::new(),
        PairAreaOverlapRatio::new(l.territory(), r.territory()),
    ))
    .fetch_all(&mut ctx)
    .await?;
```

The composition requires both slots to share the same `(L, R)` pair
tuple. The elephant-tracker `mating-pairs` demo
([`examples/elephant-tracker/src/demos/mating_pairs.rs`](https://github.com/TarunvirBains/djogi/blob/main/examples/elephant-tracker/src/demos/mating_pairs.rs))
deliberately keeps the two pair-tuple queries separate — kinship is
per-elephant-pair (`(Elephant, Elephant)`, joined with the closure of
elephant ancestries) while overlap is per-herd-pair (`(Herd, Herd)`,
on the materialised herd territories). When the natural pair-tuple
shapes differ, two queries plus a Rust-side `HashMap` keyed by herd id
is the demo pattern; the in-tuple composition above applies whenever
the kinship and overlap dimensions share the same pair shape (e.g.,
when an adopter models a per-elephant territory polygon).

### Choosing between the scalar `Expr` and the pair-side annotation

| You have... | Use |
|---|---|
| Two `Polygon` values known at query-build time | `Expr::area_of_intersection(&a, &b)` |
| Two `Polygon` columns on a joined pair-tuple | `PairAreaOverlapRatio::new(l.col(), r.col())` |

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
before your setup. Pair it with `sync_models = [...]` so the harness
projects the schema from the descriptor — no `raw_ddl`, no setup helper:

```rust
#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Place])]
async fn within_km_filters_correctly(mut ctx: djogi::DjogiContext) {
    // postgis is already installed in this per-test database, and the
    // `places` table has been synced from the Place descriptor.
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
geography — use the raw-SQL escape hatch. The `raw_*` methods live on the
sealed `djogi::__bypass::RawAccessExt` extension trait, so every call site
must decorate the enclosing item with
`#[djogi::deliberately_bypass_convention_with_raw_sql]` and pair it with an
adjacent `// JUSTIFICATION (djogi#<n>): ...` comment naming the
typed-surface gap (see [Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md)).

```rust
use djogi::prelude::*;

// Declare a custom column type in the descriptor:
// #[field(sql_type = "GEOMETRY(Polygon, 32618)")]
// pub boundary: SomeCustomType,

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): typed spatial surface is locked to GEOGRAPHY(Point, 4326);
// custom-SRID GEOMETRY columns and ST_Contains have no QuerySet equivalent.
async fn parcels_containing(
    ctx: &mut DjogiContext,
    lon: f64,
    lat: f64,
) -> djogi::Result<Vec<YourRow>> {
    let rows: Vec<YourRow> = ctx
        .raw_query(
            "SELECT id, ST_AsText(boundary) FROM parcels WHERE \
             ST_Contains(boundary, ST_Point($1, $2)::geometry)",
            &[&lon, &lat],
        )
        .await?;
    Ok(rows)
}
```

A future phase may generalize `GeoPoint` to `GeoPoint<const SRID: u32>` if
real adoption pressure emerges. That would be an additive, non-breaking change.

---

## Migration expectations

The descriptor-driven migration system consumes the spatial metadata on
`IndexSpec` and splits the GiST index into a separate
`CREATE INDEX CONCURRENTLY` step — that DDL form cannot run inside a
transaction, and PostGIS must be installed before it runs. Change the
`#[model]` struct, rebuild (`cargo build` emits the drift warning), then
run `djogi migrations compose --name add_places_location` to write
the reviewable migration pair under
`migrations/<database>/<app>/`. The composer emits the geography column
plus the GiST index in the correct transactional / non-transactional
segments — you do not hand-write `ALTER TABLE ... ADD COLUMN GEOGRAPHY` or
`CREATE INDEX CONCURRENTLY`. Library callers apply via
`djogi::migrate::apply_plan`; the
`apply` / `rollback` / `fake` / `baseline` / `verify` / `repair` CLI
dispatchers are deferred to a Phase 7 follow-up, so reach for the public
`djogi::migrate` entry points directly in the interim. See
[the migrations guide](./migrations.md) for the full contract.

The composer emits an index name following the `{table}_{column}_gix`
convention that the macro records in `IndexSpec.name` —
e.g. `places_location_gix`.

### Index metadata

The spatial GiST index emitted by `#[model(...)]` sets:

- `requires_out_of_transaction = true` — the runner places this index into a
  `CREATE INDEX CONCURRENTLY` step that runs outside any transaction.
- `extension_dependency = Some("postgis")` — the runner verifies the
  extension is installed before emitting the index DDL.

These fields live on `IndexSpec` in `ModelDescriptor::indexes`. The
`MigrationShape` contract helper (used in tests) validates that the descriptor
encodes this policy correctly.

---

## Testing spatial code

Provision PostGIS and the schema through the harness — pass `extensions =
["postgis"]` so each per-test database auto-installs the extension, and
`sync_models = [Place]` so the harness projects the schema from the
descriptor. No `setup()` helper, no `raw_ddl` calls in ordinary tests:

```rust
use djogi::prelude::*;

#[djogi::djogi_test(extensions = ["postgis"], sync_models = [Place])]
async fn within_km_filters_correctly(mut ctx: DjogiContext) {
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
- **`apply` CLI dispatcher for spatial migrations** — the descriptor-driven
  composer already emits the geography column and the
  `CREATE INDEX CONCURRENTLY` segment from `IndexSpec` metadata
  (`requires_out_of_transaction`, `extension_dependency`); applying that
  migration today goes through `djogi::migrate::apply_plan` directly. The
  `apply` / `rollback` / `fake` / `baseline` / `verify` / `repair` CLI
  dispatchers are deferred to a Phase 7 follow-up and will wrap the same
  library entry points without changing the emitted DDL.

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
