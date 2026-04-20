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
### 5.6 Underlying Engine — Native `ConditionBuilder` over `sqlx::QueryBuilder`

`QuerySet<T>` compiles its `Condition` tree into SQL via Djogi's own internal `ConditionBuilder`, a thin wrapper over `sqlx::QueryBuilder<Postgres>`. The framework does not depend on any third-party query-building crate — this layer is owned entirely by Djogi.

> **Design reference**: The community crate [`sqlx_clean_querybuilder`](https://github.com/this-ILECY/sqlx-clean-querybuilder) (MIT) serves as a reference implementation whose patterns were studied and adapted. It is not taken as a dependency — owning this code directly keeps Djogi's dependency surface lean and eliminates upstream risk for infrastructure-critical behavior.

| Layer | What it does |
|---|---|
| `QuerySet<T>` + filter closures | Developer-facing API; accumulates a typed `Condition` tree |
| `Condition` → `ConditionBuilder` | Djogi-internal: walks the tree, emits `push`/`push_bind` calls with correct `$n` numbering |
| `sqlx::QueryBuilder<Postgres>` | Manages the raw SQL buffer and positional parameter slots |
| `sqlx::query_as::<_, T>()` | Executes the built query and deserializes rows into the model type |

Developers can always drop down to raw `sqlx::QueryBuilder` directly for queries that exceed the `QuerySet` surface — Djogi is not a leaky abstraction that hides its plumbing:
```rust
use sqlx::QueryBuilder;

let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
    "SELECT * FROM vehicles WHERE make = "
);
qb.push_bind("Toyota");
qb.push(" AND gas_fill > ");
qb.push_bind(50);

let results: Vec<Vehicle> = qb.build_query_as().fetch_all(&pool).await?;
```

### 5.7 Performance Contract

The query API is expected to support efficient Postgres forms for the workload shapes Djogi targets. That means:

- expression-backed filtering and updates belong in-framework once Phase 4 lands
- aggregation, subqueries, locking, and typed result shaping are part of normal ORM work, not exceptional edge cases
- explicit eager loading must keep query counts understandable and avoid hidden N+1 behavior
- large-result evaluation must eventually support streaming/chunking rather than requiring full materialization
- generated SQL must remain inspectable, and `EXPLAIN` support belongs in the public query surface

This contract is why Djogi owns its `ConditionBuilder` and later expression IR directly instead of treating advanced query power as an optional wrapper around raw SQL.
