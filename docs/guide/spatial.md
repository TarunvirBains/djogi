> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Spatial

The `spatial` feature ships `GeoPoint` — a typed WGS-84 coordinate value with
coordinate validation, a Haversine distance helper, and transparent
`postgres_types::{ToSql, FromSql}` integration against PostGIS's
`GEOGRAPHY(Point, 4326)` column type. Two typed query methods build on the
Phase 4 expression substrate: `within_km` for radius filtering and
`order_by_distance` for distance ordering with a deterministic tiebreak.

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

- **Non-point geometries** — `LineString`, `Polygon`, `MultiPoint`, etc.
- **Arbitrary SRIDs** — `GeoPoint<const SRID: u32>` generalization.
- **Additional spatial predicates** — `ST_Contains`, `ST_Intersects`,
  `ST_Touches`, bounding-box operators, etc.
- **KNN optimization** — `ST_DWithin` index hints, `ORDER BY ... <->` operator
  for index-accelerated nearest-neighbor queries.
- **`#[djogi_test]` extension auto-provisioning** — an attribute argument to
  install PostGIS automatically in the test harness bootstrap.

---

## See also

- [Queries guide](./queries.md) — `QuerySet`, filter closures, ordering
- [Expressions guide](./expressions.md) — `Expr<T>` substrate that spatial
  predicates extend
- [Transactions guide](./transactions.md) — `atomic()` and `DjogiContext` for
  combining spatial writes with other operations
