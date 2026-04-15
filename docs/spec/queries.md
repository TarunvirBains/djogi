> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# The Query API

## 5. The Query API

### 5.1 Await Strategy

In application code, `.await` is required and explicit — standard Rust async, no surprises.

In the shell, all terminal methods block transparently via an internal `block_on`. No `.await`, no async ceremony. The developer writes the same API in both contexts; the shell just removes the noise.

### 5.2 Instance Operations
```rust
// Fetch by PK
let mut car = Vehicle::get(&pool, id).await?;

// Mutate and persist
car.gas_fill = 70;
car.save(&pool).await?;

// Delete
car.delete(&pool).await?;

// Create — struct is the input, framework populates id/created_at/updated_at
let car = Vehicle::create(&pool, Vehicle {
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
    .fetch_all(&pool).await?;
```
QuerySets are cheap to clone and compose:
```rust
let active = Vehicle::objects().filter(|f| f.active.eq(true));

let cheap = active.clone().filter(|f| f.price.lte(20_000)).fetch_all(&pool).await?;
let fast  = active.clone().filter(|f| f.horsepower.gte(300)).fetch_all(&pool).await?;
```
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
    .fetch_all(&pool).await?;
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
