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

The typed `ConditionBuilder` shape is built on `tokio-postgres + deadpool-postgres + postgres-types`, not a third-party query-builder crate.

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

### 5.7 Typed INSERT ... SELECT

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

`VALUES` inline relations as join sources and `MERGE INTO ... USING ...` are presently not supported by this surface.

### 5.7a PG18 OLD/NEW RETURNING

PostgreSQL 18 added `OLD`/`NEW` aliases in `RETURNING` clauses for `UPDATE` and `DELETE`. Djogi exposes this through:

- `Model::update_returning_pair(self, ctx) -> Result<ReturningPair<Self>>` — consuming update that returns both pre- and post-update row snapshots in a single round-trip.
- `UpdateStmt::execute_returning_pairs(ctx) -> Result<Vec<ReturningPair<T>>>` — bulk update returning one pair per affected row.
- `Model::delete_returning(self, ctx) -> Result<Self>` — consuming delete that returns the pre-delete DB snapshot.
- `QuerySet::delete_returning(ctx) -> Result<Vec<T>>` — bulk delete returning one snapshot per deleted row.

`ReturningPair<T>` has `pub old: T` and `pub new: T`. Both are non-null, fully-typed model instances decoded from the database using `FromJoinedPgRow` with the reserved `__djogi_old__` / `__djogi_new__` prefixes.

**PG18 only.** No fallback or polyfill is provided. Djogi already has a hard PG18 floor.

**Bulk memory warning.** `execute_returning_pairs` and `QuerySet::delete_returning` materialize one value per affected row. Apply `.filter(...)` to narrow the queryset before calling these terminals on large tables.

**INSERT** — `create` already returns the DB post-image; no pair type is needed. PG `OLD` is normally NULL for a simple INSERT.

**MERGE** — MERGE result hydration is presently not supported. `ReturningPair<T>` is intentionally non-optional to preserve UPDATE ergonomics.

### 5.7b VALUES Inline-Relation Joins

`InlineValues<Row>` holds a typed `Vec<Row>` of tuple data computed in Rust.
`QuerySet<T>::join_values` / `left_join_values` join it against the model
table with a structured, typed `ON` predicate via `FieldRef::eq_values` /
`DjogiField::eq_values`. `QuerySet<T>::cross_join_values` is the explicit
cartesian-product sibling when no `ON` predicate is desired.

SQL shape:

```sql
SELECT
    __djogi_m.<col>  AS <col>, ...,       -- T's COLUMN_LIST
    <alias>.<vcol_0> AS __djogi_values_0, -- projected values cols
    ...
FROM <table> AS __djogi_m
INNER JOIN (VALUES
    ($1::BIGINT, $2::DOUBLE PRECISION),   -- first row: per-column casts
    ($3, $4)                              -- subsequent rows: bare
) AS <alias>(<vcol_0>, ...)
  ON __djogi_m.<model_col> = <alias>.<vcol_0>
[WHERE __djogi_m.<filter_col> op $n]
[ORDER BY ...] [LIMIT $n] [OFFSET $n]
```

Key design properties:

- No implicit `ON TRUE` / cartesian join.  The predicate is always explicit
  on `join_values` / `left_join_values`; explicit cartesian products use
  `cross_join_values`.
- All row data binds through `SqlAccumulator::push_bind`; alias and column
  identifiers are validated with `check_user_supplied_ident` at construction.
- First-row placeholders are cast (`$1::BIGINT`) so Postgres can infer column
  types even for otherwise ambiguous NULL rows.
- Empty `InlineValues` short-circuits on inner join (no DB round-trip) and
  returns typed NULLs for the values side on left join.
- Unsupported left-queryset state (`prefetch`, `select_related`, `cache`,
  row locks, non-default `distinct`) returns `DjogiError::Validation`.
- `left_join_values` uses a framework-owned presence sentinel column to
  distinguish "no match" from "matched row with nullable columns". Result
  shape is pair-based: multiple matching VALUES rows produce multiple
  `(T, Option<Row>)` pairs for the same left row.
- Tuple rows arity 1–6.  Supported scalars: standard integers (incl. widened
  `i8/u8/u16/u32/u64`), floats, `bool`, `Decimal`, `Uuid`, `HeerId`,
  `HeerIdDesc`, `RanjId`, `RanjIdDesc`, `DateTime`, `Date`, `Time`,
  `PrimitiveDateTime`, `Interval`, `Vec<u8>`, and `Option<T>` for each.

Entry points:

- `join_values(...) -> Vec<(T, Row)>` — INNER JOIN against the inline relation.
- `left_join_values(...) -> Vec<(T, Option<Row>)>` — LEFT JOIN; unmatched rows
  decode as `None`.
- `cross_join_values(...) -> Vec<(T, Row)>` — explicit cartesian join with no
  `ON` predicate.

Adoption note: very large client-side value lists should be loaded into a
temporary/staging table instead of sent as `VALUES`; Postgres planning cost
grows with `VALUES` size.  Keep per-query VALUES under ~1 000 rows as a rule
of thumb.  The framework rejects lists where `rows × arity > 65 535`
(Postgres parameter ceiling) and also rejects terminals whose extra filter /
pagination binds would push the final query above that ceiling.

### 5.8 Performance Contract

The query API is expected to support efficient Postgres forms for the workload shapes Djogi targets. That means:

- expression-backed filtering and updates via typed `Expr<V>` handles are
  part of the framework
- aggregation, subqueries, locking, and typed result shaping are part of normal ORM work, not exceptional edge cases
- explicit eager loading must keep query counts understandable and avoid hidden N+1 behavior
- large-result evaluation must eventually support streaming/chunking rather than requiring full materialization
- generated SQL must remain inspectable, and `EXPLAIN` support belongs in the public query surface

This contract is why Djogi owns its `ConditionBuilder` and later expression IR directly instead of treating advanced query power as an optional wrapper around raw SQL.
