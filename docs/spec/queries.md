> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# The Query API

## 5. The Query API

The query layer is not only a convenience surface. It is part of Djogi's performance contract.

For common production workloads, the efficient Postgres form should be expressible in Djogi itself: explicit eager loading, set-based updates, expression-backed writes, locking, aggregation, typed result shaping, and Postgres-native operators. Raw SQL remains the escape hatch for uncommon SQL shape, not the normal way to recover performance lost to the ORM.

### 5.1 Await Strategy

In application code, `.await` is required and explicit — standard Rust async, no surprises.

In the shell, all terminal methods block transparently via an internal `block_on`. No `.await`, no async ceremony. The developer writes the same API in both contexts; the shell just removes the noise.

### 5.2 Instance Operations
```rust
// Fetch by PK
let mut car = Vehicle::get(&mut ctx, id).await?;

// Mutate and persist
car.gas_fill = 70;
car.save(&mut ctx).await?;

// Delete
car.delete(&mut ctx).await?;

// Create — struct is the input, framework populates id/created_at/updated_at
let car = Vehicle::create(&mut ctx, Vehicle {
    make: "Toyota".into(),
    model_name: "Camry".into(),
    gas_fill: 50,
    active: true,
    ..Default::default()    // framework fields — ignored on input
}).await?;
```
### 5.3 QuerySet — Lazy Builder

`QuerySet<T>` accumulates filters and options. Nothing hits the database until a terminal method is called.
```rust
let results = Vehicle::objects()
    .filter(|f| f.gas_fill.gte(69))
    .order_by(|f| f.gas_fill.desc())
    .limit(10)
    .fetch_all(&mut ctx).await?;
```
QuerySets are cheap to clone and compose:
```rust
let active = Vehicle::objects().filter(|f| f.active.eq(true));

let cheap = active.clone().filter(|f| f.price.lte(20_000)).fetch_all(&mut ctx).await?;
let fast  = active.clone().filter(|f| f.horsepower.gte(300)).fetch_all(&mut ctx).await?;
```

The query builder must make query count and SQL shape predictable. Djogi does not hide lazy loading behind field access; relation loading is always explicit (`fetch`, `prefetch`, `select_related`). If a query performs extra round trips, those round trips must be visible in the API surface.
### 5.4 Field Condition Methods

| Method | SQL equivalent |
|---|---|
| `.eq(val)` | `= $n` |
| `.neq(val)` | `!= $n` |
| `.gte(val)` | `>= $n` |
| `.gt(val)` | `> $n` |
| `.lte(val)` | `<= $n` |
| `.lt(val)` | `< $n` |
| `.in_list(vals)` | `IN ($n, ...)` |
| `.is_null()` | `IS NULL` |
| `.contains(s)` | `ILIKE '%s%'` |
| `.starts_with(s)` | `ILIKE 's%'` |
| `.between(a, b)` | `BETWEEN $n AND $m` |

Conditions are combinable inline:
```rust
.filter(|f| f.gas_fill.gte(50).and(f.active.eq(true)))
.filter(|f| f.make.eq("Toyota").or(f.make.eq("Honda")))
```
### 5.5 Programmatic Filter API

For dynamic construction and shell use where closures are unavailable:
```rust
let filter = VehicleFilter::new()
    .gas_fill(Gte(69))
    .active(Eq(true));

Vehicle::objects()
    .filter_struct(filter)
    .fetch_all(&mut ctx).await?;
```
### 5.6 Underlying Engine — Native `ConditionBuilder` over `SqlAccumulator`

`QuerySet<T>` compiles its `Condition` tree into SQL via Djogi's own internal `ConditionBuilder`, which writes through `pg::accumulator::SqlAccumulator` — a thin owned-strings + bound-values pair handed to `tokio_postgres::Client::query` at terminal time. The framework does not depend on any third-party query-building crate; this layer is owned entirely by Djogi.

> **Historical note**: The original Phase 2 implementation built on `sqlx::QueryBuilder<Postgres>`. Phase 5-Zero retired the `sqlx` substrate in favour of `tokio-postgres + deadpool-postgres + postgres-types`; the typed `ConditionBuilder` shape carried over unchanged.

| Layer | What it does |
|---|---|
| `QuerySet<T>` + filter closures | Developer-facing API; accumulates a typed `Condition` tree |
| `Condition` → `ConditionBuilder` | Djogi-internal: walks the tree, emits `push_sql` / `push_bind` calls onto a `pg::accumulator::SqlAccumulator` with correct `$n` numbering |
| `SqlAccumulator` | Owns the raw SQL string + the `Vec<Box<dyn ToSql + Sync + Send>>` bound-values buffer |
| `tokio_postgres::Client::query` | Executes the built query; rows decode through `FromPgRow` into the model type |

Developers can drop down to Djogi's raw SQL helpers for queries that exceed the `QuerySet` surface, but this is an explicit bypass that must be justified at the call site:
```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): typed QuerySet does not expose this bespoke predicate shape.
async fn toyota_with_fill(ctx: &mut DjogiContext) -> djogi::Result<Vec<Vehicle>> {
let rows = ctx.raw_query::<Vehicle>(
    "SELECT * FROM vehicles WHERE make = $1 AND gas_fill > $2",
    &[&"Toyota", &50_i32],
).await?;
Ok(rows)
}
```

For shapes outside the typed `FromPgRow` decoder (recursive CTEs, custom row tuples, scalar aggregates), `ctx.raw_scalar`, `ctx.raw_fetch_one`, and the other `RawAccessExt` helpers cover the remaining cases. Direct `tokio_postgres::Client` access is not a public `DjogiContext` API.

### 5.7 Typed INSERT ... SELECT (Phase 8.5 Cluster 4B — djogi#106)

Adopter shape — copy rows from one model's queryset into another model's
table, with closure-built column mappings:

```rust
use djogi::prelude::*;

// Archive completed orders into an archive table.
CompletedOrder::objects()
    .filter(|f| f.completed_at().lt(cutoff))
    .insert_into::<OrderArchive, _, _>(|target, source| vec![
        target.original_id().copy_from(source.id().as_insert_source()),
        target.title().copy_from(source.title().as_insert_source()),
        target.completed_at().copy_from(source.completed_at().as_insert_source()),
        // A constant column on every archived row. `InsertSelectSource::literal`
        // is polymorphic in the source model; `S` is inferred from the closure
        // return type as the enclosing source model.
        target.status().copy_from(InsertSelectSource::literal("ARCHIVED".to_string())),
    ])
    .execute(&mut ctx).await?;
```

Contract:

- The closure receives `(T::Fields, S::Fields)` and returns one or more
  [`InsertSelectColumn<S, T>`]s via
  `target_field.copy_from(source_operand)`, where `source_operand` is an
  [`InsertSelectSource<S, V>`] built from `source.col().as_insert_source()`
  or [`InsertSelectSource::literal(...)`]. Each mapping pins the target
  column's `V` to the source operand's `V` at compile time — a type
  mismatch fails to compile rather than producing a runtime Postgres type
  error.
- Source/target identity is type-checked: passing a target-side field
  where a source-side operand is required (or vice-versa) is rejected
  by the type system at the closure-return inference step, not the
  runtime emitter. See the pinned compile-fail fixtures under
  `djogi/tests/compile_fail/insert_select_*` for the negative cases.
- The target's framework columns (`id`, `created_at`, `updated_at`) are
  populated by their column-level `DEFAULT` clauses — the emitter never
  names them unless the closure explicitly maps them. Matches
  `Model::create`'s contract.
- The terminal returns the affected row count.
- WHERE / ORDER BY / LIMIT / OFFSET on the source are emitted into the
  SELECT side; `QuerySet::none()` short-circuits to `Ok(0)`.
- The terminal returns `DjogiError::Validation` when the source carries
  state that cannot be safely represented in INSERT...SELECT
  (`prefetch`, `select_related`, `cache`, a non-default `LockMode`, or
  a non-default `DistinctMode`).

Related framework gaps not covered by this surface:

- Set operations (`UNION` / `INTERSECT` / `EXCEPT`) — djogi#101.
- `LATERAL` joins — djogi#102.
- `VALUES` inline relations as join sources — djogi#103.
- `MERGE INTO ... USING ...` — djogi#178.
- PG18 `OLD` / `NEW` in `RETURNING` — djogi#180.
- `RETURNING` for INSERT...SELECT — follow-up issue; current terminal
  returns the affected row count only.

### 5.8 Performance Contract

The query API is expected to support efficient Postgres forms for the workload shapes Djogi targets. That means:

- expression-backed filtering and updates belong in-framework once Phase 4 lands
- aggregation, subqueries, locking, and typed result shaping are part of normal ORM work, not exceptional edge cases
- explicit eager loading must keep query counts understandable and avoid hidden N+1 behavior
- large-result evaluation must eventually support streaming/chunking rather than requiring full materialization
- generated SQL must remain inspectable, and `EXPLAIN` support belongs in the public query surface

This contract is why Djogi owns its `ConditionBuilder` and later expression IR directly instead of treating advanced query power as an optional wrapper around raw SQL.
