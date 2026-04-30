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

Developers can always drop down to raw `tokio-postgres` directly for queries that exceed the `QuerySet` surface — Djogi is not a leaky abstraction that hides its plumbing:
```rust
let rows = ctx.raw_query::<Vehicle>(
    "SELECT * FROM vehicles WHERE make = $1 AND gas_fill > $2",
    &[&"Toyota", &50_i32],
).await?;
```

For shapes outside the typed `FromPgRow` decoder (recursive CTEs, custom row tuples, scalar aggregates), `ctx.raw_scalar` and `ctx.raw_fetch_one` cover the remaining cases, and direct access to `tokio_postgres::Client` is one `match ctx.inner_mut()` away.

### 5.7 Performance Contract

The query API is expected to support efficient Postgres forms for the workload shapes Djogi targets. That means:

- expression-backed filtering and updates belong in-framework once Phase 4 lands
- aggregation, subqueries, locking, and typed result shaping are part of normal ORM work, not exceptional edge cases
- explicit eager loading must keep query counts understandable and avoid hidden N+1 behavior
- large-result evaluation must eventually support streaming/chunking rather than requiring full materialization
- generated SQL must remain inspectable, and `EXPLAIN` support belongs in the public query surface

This contract is why Djogi owns its `ConditionBuilder` and later expression IR directly instead of treating advanced query power as an optional wrapper around raw SQL.
