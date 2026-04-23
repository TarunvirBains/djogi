> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Grouped Aggregation and Window Aggregates

Phase 6.5 adds Djogi's typed grouping layer on top of Phase 4's expression
substrate and Phase 4's `AggregateExpr<V>` annotations. Every entry point
is a method on `QuerySet<T>` that transitions into a
`GroupedQuerySet<T, K>`; calling `.annotate(...)` on that transitions once
more into a terminal `GroupedAnnotatedQuerySet<T, K, A>` that can be
fetched.

The surface is **default-feature** — no opt-in required. The spatial
grouping entry points documented at the end of this guide are gated on the
[`spatial` feature flag](./spatial.md).

---

## The shape

Each grouped query walks through three type-state stages:

```text
QuerySet<T>
    │  .group_by(...)  / .rollup(...) / .cube(...) / .group_by_sets(...)
    ▼
GroupedQuerySet<T, K>
    │  .annotate(|f| ...)                 — attach one or more aggregates
    ▼
GroupedAnnotatedQuerySet<T, K, A>
    │  .having(|k, a| Expr<bool>)         — filter aggregated groups
    │  .order_by(|k, a| OrderExpr)        — order the grouped result
    │  .limit(n) / .offset(n)             — paginate the grouped result
    │  .fetch_all(&mut ctx)
    ▼
Vec<(K::Decoded, A::Decoded)>
```

`.having` and `.order_by` receive **two** arguments: the key tuple `k`
and the aggregate tuple `a`. See "Ordering and pagination" and
"`HAVING` — filtering groups" below for the closure shape and for the
deferred aggregate-bridge caveat.

The key tuple `K` and the aggregate tuple `A` are both sealed trait objects
— users never name them directly. Each terminal returns a `Vec` of
`(key_tuple, aggregate_tuple)` pairs, decoded positionally.

---

## Grouping by a single column

```rust
use djogi::prelude::*;

// ORGANIZATIONS × SUM(AMOUNT)
let totals: Vec<(i64, i64)> = Order::objects()
    .group_by(|f| f.org_id())
    .annotate(|f| f.amount().sum())
    .fetch_all(&mut ctx)
    .await?;

for (org_id, total) in totals {
    println!("org {org_id} total = {total}");
}
```

> **Emitted SQL:**
> ```sql
> SELECT org_id, SUM(amount) AS __djogi_agg_0
> FROM orders AS t
> GROUP BY org_id
> ```

Single-column keys decode to a bare scalar (`i64` in the example), not a
one-tuple. The emitter pushes the column name unqualified in the SELECT
and GROUP BY lists because there is no ambiguity under a single-table
query; the `FROM <table> AS t` alias is retained for WHERE / ORDER BY
fragments that the substrate inherits from `QuerySet<T>`.

### Ordering and pagination

`.order_by(...)` on a grouped queryset receives a closure of the form
`|k, a| -> OrderExpr`, where `k` is the group-key tuple and `a` is the
aggregate tuple. For a single-column key, `k` is the `FieldRef` itself,
so `k.asc()` / `k.desc()` produce an `OrderExpr` directly. `.limit` /
`.offset` paginate the grouped result:

```rust
let top_three_orgs: Vec<(i64, i64)> = Order::objects()
    .group_by(|f| f.org_id())
    .annotate(|f| f.amount().sum())
    .order_by(|k, _a| k.desc())
    .limit(3)
    .fetch_all(&mut ctx)
    .await?;
```

**Deferred:** ordering directly by an aggregate expression
(`.order_by(|k, a| a.desc())`) is not available in Phase 6.5 —
`AggregateExpr<V>` has no `.asc()` / `.desc()` methods and no
`Into<Expr<V>>` bridge, so the aggregate cannot compose into the
ORDER BY slot through the typed surface. For top-N-by-aggregate
queries today, sort client-side after `fetch_all` or drop to
`ctx.raw_query(...)`.

### `HAVING` — filtering groups

`.having(...)` also receives a `|k, a| -> Expr<bool>` closure. For a
single-column key, lift the `FieldRef` into the expression substrate
with `k.as_expr()` and then use the comparison methods on `Expr<V>`
(`.gte`, `.lt`, `.eq`, …):

```rust
let big_orgs: Vec<(i64, i64)> = Order::objects()
    .group_by(|f| f.org_id())
    .annotate(|f| f.amount().sum())
    .having(|k, _a| k.as_expr().gte(Expr::literal(2_i64)))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT org_id, SUM(amount) AS __djogi_agg_0
> FROM orders AS t
> GROUP BY org_id
> HAVING org_id >= $1
> ```

**Deferred:** filtering on an aggregate expression
(`.having(|k, a| a.gt(Expr::literal(10_000_i64)))`) is not available
in Phase 6.5 for the same reason as aggregate-based ordering — it
requires the same `AggregateExpr<V>` → `Expr<V>` bridge. Filter
client-side, or write the HAVING predicate through `ctx.raw_query(...)`
until the bridge lands.

---

## Grouping by multiple columns (tuple keys)

Return a Rust tuple from the `group_by` closure and the grouped output
decodes as a tuple:

```rust
let by_org_region: Vec<((i64, String), i64)> = Order::objects()
    .group_by(|f| (f.org_id(), f.region_code()))
    .annotate(|f| f.amount().sum())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT org_id, region_code, SUM(amount) AS __djogi_agg_0
> FROM orders AS t
> GROUP BY org_id, region_code
> ```

Djogi supports tuple keys up to arity 4 in 6.5 (the same sealed
`IntoGroupKeyTuple` trait is implemented for arity 1/2/3/4). Exceeding the
arity cap raises a clear compile error.

---

## ROLLUP, CUBE, and GROUPING SETS

Each of the three extended GROUP BY shapes is its own entry point —
`.rollup(...)`, `.cube(...)`, `.group_by_sets(...)` — and each wraps the
key column list in the matching SQL syntax.

```rust
// Hierarchical subtotals: (org), (org, region), grand total row.
let rollup: Vec<((Option<i64>, Option<String>), i64)> = Order::objects()
    .rollup(|f| (f.org_id(), f.region_code()))
    .annotate(|f| f.amount().sum())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> GROUP BY ROLLUP (org_id, region_code)
> ```

Extended grouping returns `NULL` for the "totals" rows; Djogi decodes those
columns as `Option<K>` automatically. `Option<i64>::None` in the key tuple
signals a grand total (for ROLLUP) or a null axis (for CUBE / GROUPING
SETS).

```rust
// Explicit set list: (org), (region), (org, region).
Order::objects()
    .group_by_sets(|f| [
        (Some(f.org_id()), None),
        (None, Some(f.region_code())),
        (Some(f.org_id()), Some(f.region_code())),
    ])
    .annotate(|f| f.amount().sum());
```

> **Emitted `GROUP BY` shape:**
> ```sql
> GROUP BY GROUPING SETS ((org_id), (region_code), (org_id, region_code))
> ```

**Deferred in Phase 6.5:** typed `.fetch_all` on a unit-key (arity-0)
`group_by_sets` — the SELECT list emits a stray leading comma when the
key-tuple emission produces zero columns (`SELECT , SUM(amount) AS
__djogi_agg_0 FROM ...`). Until the empty-key SELECT path is fixed, use
either (a) a non-empty typed key tuple that positionally matches every
declared grouping set, or (b) `ctx.raw_query(...)` for the composite
`GROUPING SETS` output. See the integration test file
`tests/integration/phase6_5_aggregates.rs` for the issue's full repro.

---

## Multi-aggregate annotate

Return a tuple from the `annotate` closure to compute multiple aggregates
in one pass. The output tuple type matches positionally:

```rust
let stats: Vec<(i64, (i64, i64, Option<i64>))> = Order::objects()
    .group_by(|f| f.org_id())
    .annotate(|f| (
        f.id().count_star(),     // i64
        f.amount().sum(),         // i64
        f.amount().max(),         // Option<i64> — MAX over empty group is NULL
    ))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT org_id,
>        COUNT(*)     AS __djogi_agg_0,
>        SUM(amount)  AS __djogi_agg_1,
>        MAX(amount)  AS __djogi_agg_2
> FROM orders AS t
> GROUP BY org_id
> ```

Arity 1/2/3/4 tuples are supported. Aggregate-aggregate collisions on
synthetic aliases are prevented — Djogi names each aggregate slot
`__djogi_agg_N` (N starting at 0) and rejects user-supplied SELECT aliases
whose name begins with `__djogi_agg_` at SQL-build time (diagnostic:
`DjogiError::AnnotationAliasCollision`).

---

## `.annotate(...)` without `group_by` — window aggregates

Annotating a `QuerySet<T>` *without* calling `group_by` first emits each
aggregate as a window function with `OVER ()`. Every row in the result set
carries the table-wide aggregate value.

```rust
// Each row carries the table-wide total as an extra column.
let rows: Vec<(Order, i64)> = Order::objects()
    .annotate(|f| f.amount().sum())
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT t.*, SUM(amount) OVER () AS __djogi_agg_0
> FROM orders AS t
> ```

### Custom window frames

`AggregateExpr<V>::over(|w| ...)` takes a closure that receives a
`WindowBuilder`. Chain `.partition_by(f.col())` / `.order_by(f.col())`
(columns, not closures) to build the window spec. Both methods can be
called multiple times to append additional terms.

```rust
let running: Vec<(Order, i64)> = Order::objects()
    .annotate(|f| f.amount().sum().over(
        |w| w.partition_by(f.org_id()).order_by(f.created_at())
    ))
    .fetch_all(&mut ctx)
    .await?;
```

> **Emitted SQL:**
> ```sql
> SELECT t.*,
>        SUM(amount) OVER (
>            PARTITION BY org_id
>            ORDER BY created_at ASC
>        ) AS __djogi_agg_0
> FROM orders AS t
> ```

Frame clauses (`ROWS`, `RANGE`, `GROUPS`) are available on the builder —
see the [expressions guide](./expressions.md) for the full `WindowBuilder`
API including frame bounds and `EXCLUDE` variants.

### DISTINCT aggregates

`AggregateExpr<V>::distinct()` prefixes the aggregate's argument with
`DISTINCT` for the `COUNT` / `SUM` / `AVG` aggregates that accept it:

```rust
let distinct_customers: i64 = Order::objects()
    .annotate(|f| f.customer_id().count().distinct())
    .fetch_all(&mut ctx)
    .await?
    .first()
    .map(|(_, c)| *c)
    .unwrap_or(0);
```

Combining `DISTINCT` with a window frame is rejected at build time —
Postgres does not support `DISTINCT` inside windowed aggregates — and
raises `DjogiError::UnsupportedAggregate` with an explanation of the
combination.

---

## Spatial grouping (feature = "spatial")

Phase 6.5 adds three spatial grouping entry points that reuse the same
grouped substrate. Each produces a typed key whose `Option<...>` slot
captures rows that do not match any group (the spatial analogue of the
`NULL` bucket under ROLLUP / CUBE).

These are gated on the [`spatial` feature](./spatial.md).

### `group_by_region` / `count_by_region`

Spatial point-in-polygon JOIN:

```rust
use djogi::prelude::*;
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

`RegionKey<Neighborhood>::region_pk` is `Option<HeerId>`; the `None`
bucket captures stores whose `location` does not fall inside any
`neighborhoods` polygon. The `LEFT JOIN` preserves those rows instead of
silently dropping them.

**`ST_Covers` vs `ST_Contains`:** PostGIS 3.x only defines
`ST_Contains(geometry, geometry)`, not a geography overload. Djogi stores
spatial columns as `GEOGRAPHY(..., 4326)`, so the JOIN uses `ST_Covers`
instead — it has a native geography overload, equivalent semantics for
the point-in-polygon case, and keeps GiST-indexed bbox prefiltering active.
See `docs/spec/decisions.md`.

`count_by_region` is scalar-count sugar — it calls
`group_by_region(...).annotate(|f| f.id().count_star())` internally.

#### Missing-GiST guard

If either side of the JOIN uses a geography column with no declared GiST
index in its `ModelDescriptor::indexes`, Djogi emits a one-shot
`tracing::warn!` on the `"djogi::spatial"` target. The guard is a
`std::sync::Once` — repeat calls across the process lifetime do not
re-warn.

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

`ClusterId` is `ClusterId(Option<i32>)` — `Some(n)` for assigned clusters,
`None` for noise points (DBSCAN's "outliers").

`ClusterRadius` is a builder:

- `ClusterRadius::meters(f64)` — converts the radius to a degree-based
  `eps` using the equator circumference (`(2·π·R_earth)/360` with WGS84
  `R_earth = 6,378,137 m`). At high latitudes the earth-curvature
  approximation degrades; the `min_points(i32)` knob tempers the impact.
- `ClusterRadius::degrees(f64)` — raw degree value for callers who want
  direct control.
- `.min_points(n: i32)` — DBSCAN's minimum cluster population.

**Why the subquery wrap?** Postgres rejects `ST_ClusterDBSCAN(...) OVER
() ... GROUP BY cluster_id` with `ERROR: window functions are not
allowed in GROUP BY`. The emitter materialises `cluster_id` in an inner
subquery so the outer `GROUP BY` references a plain column.

### `bucket_by_cell` — geohash

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

`GeohashPrecision::{P1 .. P12}` corresponds to the standard geohash
precision levels. Each `P` step increases resolution by roughly
4×— `P5` ≈ 4.9 km × 4.9 km cells at the equator, `P7` ≈ 150 m, `P9` ≈ 5 m.

`GeohashKey` is `GeohashKey(Option<String>)` — `None` covers rows whose
geometry is NULL, mirroring the NULL-symmetry of `ClusterId`.

---

## Terminal semantics

Every grouped terminal follows the same contract:

- **`fetch_all(&mut ctx)`** returns `Vec<(K::Decoded, A::Decoded)>` in the
  order imposed by `.order_by(...)`, or in an unspecified order if none
  was given.
- **Empty groups** simply do not appear in the output — Postgres GROUP BY
  semantics.
- **`Option<V>` return types** — aggregates whose argument can be NULL over
  an empty group (e.g. `MAX`, `MIN`, `AVG`) are typed as
  `AggregateExpr<Option<V>>`. `COUNT(*)` / `COUNT(col)` are always typed
  as `AggregateExpr<i64>`.
- **Legality checks** — DISTINCT-with-window combinations and similar
  illegal combinations are caught before SQL emission and return
  `DjogiError::UnsupportedAggregate`.

---

## See also

- [Queries guide](./queries.md) — `QuerySet`, filter closures, ordering
- [Expressions guide](./expressions.md) — `Expr<T>`, `AggregateExpr<V>`,
  `Window`, frame clauses
- [Spatial guide](./spatial.md) — feature-gated spatial primitives and
  the three spatial grouping entry points
