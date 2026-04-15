> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Djogi Implementation Plan

*Sequenced to reach production-readiness for a real-world Axum + Postgres application with ~36 entities, complex queries, PostGIS, and SeaORM migration path.*

---

## Guiding Principles

1. **Each phase produces a usable, testable crate** — not a waterfall of unshippable code
2. **The proc macro (`djogi-macros`) is the foundation** — everything derives from `#[derive(Model)]`
3. **Raw SQL escape hatch ships in Phase 1** — the framework must never trap the developer
4. **Tests against real Postgres** — no mocking the database; use `sqlx::test` fixtures
5. **Postgres-only from day one** — every SQL string targets Postgres directly

---

## Phase 0: Workspace Setup

**Goal:** Cargo workspace compiles, CI runs, empty crates are published internally.

- [ ] Initialize Cargo workspace with 4 crates: `djogi`, `djogi-macros`, `djogi-cli`, `djogi-shell`
- [ ] Set up `Cargo.toml` workspace dependencies: `sqlx` (postgres, runtime-tokio-rustls), `tokio`, `serde`, `serde_json`, `time`, `heeranjid`
- [ ] Set up CI (GitHub Actions): `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check`
- [ ] Add `docker-compose.yml` with Postgres 16 for local dev and CI test databases
- [ ] Create `Djogi.toml` config loader skeleton via `figment`
- [ ] Establish integration test infrastructure: test database creation/teardown, `sqlx::test` macros

**Deliverable:** `cargo build` succeeds, CI green, Postgres test DB spins up.

---

## Phase 1: Core Model System

**Goal:** Define structs with `#[derive(Model)]`, get CRUD operations against Postgres.

### 1a: The Proc Macro — `#[derive(Model)]`

- [ ] Parse `#[model(table = "...")]` attribute
- [ ] Inject framework fields: `id: HeerId`, `created_at: OffsetDateTime`, `updated_at: OffsetDateTime`
- [ ] Generate `impl Model for T` with: `table_name()`, `descriptor()`
- [ ] Generate `impl sqlx::FromRow for T`
- [ ] Generate `ModelDescriptor` and register via `inventory::submit!`
- [ ] Support `#[model(pk = "serial")]` for auto-increment i32 PKs
- [ ] Support `#[model(pk = "ranjid")]` for UUID PKs via RanjId
- [ ] Support `#[model(pk = "none")]` for custom PK fields (user-declared)
- [ ] Support composite primary keys: `#[model(pk = ["field_a", "field_b"])]`

### 1b: Field Types

- [ ] `String` → `TEXT`
- [ ] `i16` / `i32` / `i64` → `SMALLINT` / `INTEGER` / `BIGINT`
- [ ] `f32` / `f64` → `REAL` / `DOUBLE PRECISION`
- [ ] `bool` → `BOOLEAN`
- [ ] `OffsetDateTime` → `TIMESTAMPTZ` (via `time` crate)
- [ ] `NaiveDate` → `DATE`
- [ ] `rust_decimal::Decimal` → `NUMERIC`
- [ ] `uuid::Uuid` → `UUID`
- [ ] `Option<T>` → nullable
- [ ] `serde_json::Value` → `JSONB` (untyped JSON — distinct from `Jsonb<T>`)
- [ ] `Vec<T>` → Postgres arrays (`TEXT[]`, `INTEGER[]`, etc.)
- [ ] `HeerId` / `RanjId` → `BIGINT` / `UUID` with serde as string

### 1c: CRUD Operations

- [ ] `T::get(pool, id)` → `SELECT * FROM table WHERE id = $1`
- [ ] `T::create(pool, instance)` → `INSERT ... RETURNING *` (full model returned)
- [ ] `instance.save(pool)` → `UPDATE ... SET ... WHERE id = $1` (all fields or dirty-tracked)
- [ ] `instance.delete(pool)` → `DELETE FROM table WHERE id = $1`
- [ ] `instance.refresh_from_db(pool)` → `SELECT * FROM table WHERE id = $1` reload

### 1d: Raw SQL Escape Hatch

- [ ] `djogi::raw::query_as::<T>(pool, sql, params)` → execute raw SQL, return typed model
- [ ] `djogi::raw::query_scalar::<T>(pool, sql, params)` → return scalar value
- [ ] `djogi::raw::execute(pool, sql, params)` → execute without return
- [ ] All raw methods accept both `&PgPool` and `&mut Transaction` (generic over connection)

### 1e: Connection Generics

- [ ] Define `trait DjogiConnection` implemented for `&PgPool` and `&mut Transaction<'_, Postgres>`
- [ ] All CRUD methods generic over `impl DjogiConnection` — same code works with pool or transaction

**Deliverable:** Define a struct, derive Model, create/read/update/delete against Postgres. Raw SQL works.

---

## Phase 2: Query Builder

**Goal:** Typed, composable QuerySet with filter/order/limit and terminal methods.

### 2a: QuerySet Core

- [ ] `T::objects()` returns `QuerySet<T>` (lazy, nothing hits DB until terminal method)
- [ ] `.filter(|f| f.field.eq(val))` — typed closure-based filtering
- [ ] `.exclude(|f| f.field.eq(val))` — negative filtering (NOT WHERE)
- [ ] `.order_by(|f| f.field.asc())` / `.desc()` / `.asc().nulls_last()`
- [ ] `.limit(n)` / `.offset(n)`
- [ ] `.distinct()` — with Postgres `DISTINCT ON(fields)` support

### 2b: Terminal Methods

- [ ] `.fetch_all(pool)` → `Vec<T>`
- [ ] `.fetch_one(pool)` → `T` (error if not exactly one)
- [ ] `.first(pool)` → `Option<T>`
- [ ] `.count(pool)` → `i64`
- [ ] `.exists(pool)` → `bool`
- [ ] `.explain(pool)` → `String` (EXPLAIN output)
- [ ] `.none()` → empty QuerySet (never queries)

### 2c: Lookup Methods on Field References

- [ ] `eq`, `neq`, `gt`, `gte`, `lt`, `lte`
- [ ] `in_list`, `not_in_list`
- [ ] `is_null`, `is_not_null`
- [ ] `contains` (ILIKE `%val%`), `icontains`
- [ ] `starts_with`, `istarts_with`
- [ ] `ends_with`, `iends_with`
- [ ] `between`
- [ ] `iexact` (case-insensitive exact)
- [ ] `regex`, `iregex` (Postgres `~` / `~*`)
- [ ] `.and()` / `.or()` for combining conditions

### 2d: Programmatic Filter API (for dynamic/shell use)

- [ ] `{Model}Filter::new().field(Op::Eq(val))` — struct-based, no closures
- [ ] `.filter_struct(filter)` on QuerySet

### 2e: Bulk Operations on QuerySet

- [ ] `.update(|f| f.field.set(val))` → `UPDATE ... SET ... WHERE ...` (returns row count)
- [ ] `.delete()` → `DELETE FROM ... WHERE ...` (returns row count)

### 2f: ConditionBuilder Internals

- [ ] `Condition` enum tree: `And`, `Or`, `Not`, `Leaf(field, op, value)`
- [ ] Walk tree → emit positional `$n` parameters via `sqlx::QueryBuilder<Postgres>`
- [ ] Correct parenthesization for nested AND/OR
- [ ] `IN (...)` expansion for variable-length lists

**Deliverable:** Full query builder with typed filters, all lookups, bulk update/delete.

---

## Phase 3: Relations

**Goal:** ForeignKey, prefetch, select_related, M2M with through models.

### 3a: ForeignKey

- [ ] `ForeignKey<T>` field type → `BIGINT REFERENCES table(id)`
- [ ] `#[field(on_delete = "cascade|restrict|set_null|set_default|protect|do_nothing")]`
- [ ] `.fetch(pool)` → single query to load related object
- [ ] `.resolved()` → `Option<&T>` after prefetch (panics with helpful message if not prefetched)
- [ ] Support `Option<ForeignKey<T>>` for nullable FKs

### 3b: Prefetch (separate queries)

- [ ] `.prefetch(ModelRelated::relation_name())` on QuerySet
- [ ] Collects PKs from results, fires one `IN (...)` query per relation, stitches back
- [ ] Support chained prefetch through FK chains

### 3c: Select Related (JOIN-based)

- [ ] `.select_related(|f| f.owner)` on QuerySet → `LEFT JOIN` in single query
- [ ] Support chaining: `.select_related(|f| (f.owner, f.fuel_type))`
- [ ] Populated relations accessible via `.resolved()` after fetch

### 3d: Many-to-Many (explicit through models)

- [ ] `#[model(through)]` attribute on junction table models
- [ ] `impl ManyToMany<Target> for Source` with `type Through` and `RELATION` const
- [ ] Generated convenience methods: `.groups()`, `.add_to_group()`, `.remove_from_group()`

**Deliverable:** FK with all cascade options, prefetch + select_related, M2M through models.

---

## Phase 4: Transactions & Expressions

**Goal:** Application-level transactions and field expressions for complex queries.

### 4a: Transaction API

- [ ] `djogi::transaction::atomic(pool, |txn| async { ... })` — closure-based, returns `Result`
- [ ] Manual: `let txn = pool.begin().await?; ... txn.commit().await?;`
- [ ] Savepoint support within transactions
- [ ] `on_commit(pool, || { ... })` — callbacks that fire only after outermost commit
- [ ] Savepoint-aware callback tracking (rollback discards inner callbacks)
- [ ] `select_for_update()` on QuerySet → `FOR UPDATE` (with `nowait`, `skip_locked`)

### 4b: Field Expressions

- [ ] `Expr::field(|f| f.price)` — typed reference to another column (F-equivalent)
- [ ] Arithmetic: `Expr::field(|f| f.price) + Expr::val(10)` → `price + 10`
- [ ] Use in update: `.update(|f| f.price.set(f.price + Expr::val(10)))`
- [ ] Use in filter: `.filter(|f| f.price.gt(f.cost))`
- [ ] `Expr::val(literal)` — literal value wrapper

### 4c: Aggregation

- [ ] `.aggregate(Count::all())` → terminal, returns `i64`
- [ ] `.aggregate(Sum(|f| f.price))` → returns `Option<Decimal>`
- [ ] `Avg`, `Max`, `Min`, `Count` (with distinct support)
- [ ] `.annotate("total", Sum(|f| f.price))` — add computed column to results
- [ ] Aggregate `FILTER (WHERE ...)` clause — Postgres-native

### 4d: Subqueries & Conditional Expressions

- [ ] `Subquery(queryset)` — use a QuerySet as a subquery expression
- [ ] `Exists(queryset)` — `EXISTS(SELECT ...)` in filter
- [ ] `OuterRef(|f| f.id)` — correlated subquery reference
- [ ] `Expr::case().when(cond, then_val).when(cond2, then_val2).default(else_val)` — CASE/WHEN

### 4e: Convenience Methods

- [ ] `T::get_or_create(pool, lookup_fields, defaults)` → `(instance, created: bool)`
- [ ] `T::update_or_create(pool, lookup_fields, defaults)` → `(instance, created: bool)`
- [ ] `T::in_bulk(pool, ids)` → `HashMap<PK, T>`
- [ ] `T::bulk_create(pool, vec, on_conflict)` — batch insert with conflict handling
- [ ] `T::bulk_update(pool, vec, fields)` — batch update via CASE/WHEN
- [ ] `T::bulk_upsert(pool, vec, conflict_fields, update_fields)` — INSERT ON CONFLICT DO UPDATE

**Deliverable:** Full transaction support, field expressions, aggregation, subqueries, bulk upsert.

---

## Phase 5: Postgres-Native Features

**Goal:** First-class support for Postgres features that other ORMs treat as optional.

### 5a: Postgres Enum Types

- [ ] `#[derive(DjogiEnum)]` on Rust enums → Postgres `CREATE TYPE ... AS ENUM`
- [ ] Auto-map between Rust enum variants and Postgres enum values
- [ ] Support string-backed enums (`#[djogi_enum(as_string)]`) for schema evolution flexibility
- [ ] Migration support: `ALTER TYPE ... ADD VALUE`

### 5b: Postgres Array Fields

- [ ] `Vec<String>` → `TEXT[]`, `Vec<i32>` → `INTEGER[]`, etc.
- [ ] Array lookups: `contains` (`@>`), `contained_by` (`<@`), `overlap` (`&&`)
- [ ] Array length: `.len()` lookup
- [ ] Array index access in expressions: `field[0]`

### 5c: Typed JSONB (`Jsonb<T>`)

- [ ] `Jsonb<T>` with typed schema + unknown field preservation (per existing spec)
- [ ] JSONB path lookups: `has_key` (`?`), `has_keys` (`?&`), `has_any_keys` (`?|`)
- [ ] JSONB containment: `contains` (`@>`), `contained_by` (`<@`)
- [ ] Subfield query filters via proc macro (per existing spec)
- [ ] Validation on save with dot-notation error paths

### 5d: Postgres-Native Aggregates

- [ ] `ArrayAgg(field)` → `ARRAY_AGG()` with ordering and distinct
- [ ] `JsonAgg(field)` → `JSONB_AGG()`
- [ ] `StringAgg(field, delimiter)` → `STRING_AGG()`
- [ ] `BoolAnd` / `BoolOr` → `BOOL_AND()` / `BOOL_OR()`

### 5e: Postgres-Native Indexes

- [ ] `#[index(gin)]` / `#[index(gist)]` / `#[index(brin)]` — index type annotations
- [ ] `OpClass` support for trigram and JSONB indexing
- [ ] Partial indexes: `#[index(condition = "active = true")]`
- [ ] Covering indexes: `#[index(include = ["field"])]`

### 5f: Database Functions

- [ ] Comparison: `Coalesce`, `Greatest`, `Least`, `NullIf`, `Cast`
- [ ] Text: `Lower`, `Upper`, `Trim`, `Concat`, `Replace`, `Substr`, `Length`
- [ ] DateTime: `Now`, `Extract`, `TruncDate`
- [ ] Math: `Abs`, `Ceil`, `Floor`, `Round`

**Deliverable:** Postgres enums, arrays, typed JSONB, native aggregates, indexes, database functions.

---

## Phase 6: Migration System

**Goal:** Build-time drift detection, SQL generation, apply/rollback.

### 6a: Schema Differ

- [ ] `ModelDescriptor` comparison: detect added/removed/altered fields, tables, indexes
- [ ] `#[field(renamed_from = "old")]` for rename detection
- [ ] `#[model(renamed_from = "old_table")]` for table rename detection
- [ ] Destructive operation gating with `--allow-destructive`

### 6b: SQL Generation

- [ ] Generate up/down SQL pairs from `SchemaDelta`
- [ ] `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE ADD/DROP/ALTER COLUMN`
- [ ] `CREATE INDEX` / `DROP INDEX` (with `CONCURRENTLY` support)
- [ ] `CREATE TYPE ... AS ENUM` / `ALTER TYPE ... ADD VALUE`
- [ ] `ADD CONSTRAINT` / `DROP CONSTRAINT` (with `NOT VALID` + `VALIDATE` support)
- [ ] Foreign key constraints with cascade options

### 6c: Build-Time Integration

- [ ] `build.rs` reads `target/djogi_models.json` and diffs against `schema_snapshot.json`
- [ ] Emits compiler warning (not error) when drift detected
- [ ] Writes migration SQL files to `migrations/`

### 6d: CLI

- [ ] `cargo djogi migrate` — apply pending, update snapshot
- [ ] `cargo djogi migrate rollback` — roll back last migration
- [ ] `cargo djogi migrate show NNNN` — display SQL without running
- [ ] `cargo djogi makemigrations` — manual trigger with `--dry-run`, `--allow-destructive`
- [ ] `cargo djogi migrate --fake NNNN` — mark applied without running
- [ ] `cargo djogi db reset` — drop + recreate + migrate (dev only, triple-gated)
- [ ] `cargo djogi db seed` — run `seeds.rhai`

### 6e: Data Migrations

- [ ] Support raw SQL data migrations (hand-written `.sql` files in `migrations/`)
- [ ] Support Rhai script data migrations (`.rhai` files using shell model API)

**Deliverable:** Full migration system with drift detection, SQL generation, CLI, data migrations.

---

## Phase 7: Model Hooks, Composition & Proxy

**Goal:** Lifecycle hooks, abstract model composition, proxy models.

### 7a: Trait-Based Model Hooks

- [ ] `impl ModelHooks for T` with `before_create`, `after_create`, `before_save`, `after_save`, `before_delete`, `after_delete`
- [ ] Hooks receive `&mut self` (before) or `&self` (after) + connection reference
- [ ] Optional — models without `impl ModelHooks` have zero hook overhead

### 7b: Abstract Model Composition

- [ ] `#[derive(Auditable)]` — injects `created_at`, `updated_at` (already exists) + `created_by: Option<String>`
- [ ] `#[derive(SoftDeletable)]` — injects `deleted_at: Option<OffsetDateTime>`, adds default filter excluding deleted
- [ ] Custom field group macros: developers can define their own derive macros that inject fields
- [ ] Constraint/index name interpolation: `%(model)s_%(field)s_unique`

### 7c: Proxy Models

- [ ] `#[model(proxy_for = "Vehicle")]` — shares parent table, different Rust type
- [ ] Custom default ordering on proxy
- [ ] Custom default filter on proxy (e.g., `WHERE active = true`)
- [ ] Different `ModelHooks` on proxy vs parent

### 7d: Computed Queryable Properties

- [ ] `#[computed(sql = "base_price * (1.0 + tax_rate)")]` — Rust getter + SQL expression
- [ ] Usable in `.filter()`, `.order_by()`, `.annotate()` — the macro wires both sides
- [ ] Not stored in DB unless `#[computed(sql = "...", stored)]` (Postgres GENERATED ALWAYS AS)

**Deliverable:** Lifecycle hooks, composable field groups, proxy models, computed properties.

---

## Phase 8: Shell & Admin

**Goal:** Interactive Rhai REPL and auto-generated admin panel.

### 8a: Shell (Rhai REPL)

- [ ] `cargo djogi shell` — launches REPL with all models loaded
- [ ] Synchronous API via `block_on()` — no `.await` in shell
- [ ] `pp(value)`, `sql("...")`, `begin()`, `commit()`, `rollback()`, `savepoint()`
- [ ] Error handling: one-liner + full traceback to `.djogi_shell_errors/`
- [ ] `.export` / `.import` / `.bookmark` for session scripts
- [ ] `cargo djogi shell --run script.rhai` for headless execution

### 8b: Admin Panel (HTMX + Askama)

- [ ] Auto-generate list view from `ModelDescriptor` with pagination, sorting, search
- [ ] Auto-generate CRUD forms from field metadata
- [ ] HTMX-driven: partial page updates for pagination, filtering, inline editing
- [ ] `admin` feature flag — opt-in dependency
- [ ] Annotation-driven customization: `#[admin(list_display = [...])]`
- [ ] Trait-based advanced customization: `impl AdminConfig for T`

**Deliverable:** Working shell and admin panel.

---

## Phase 9: CRUD Logging & Observability

**Goal:** Automated audit trail and event logging.

- [ ] Three-database architecture: app, crud_logs, event_logs
- [ ] Per-model `#[model(crud_log = true)]` — auto-provision mirror `_logs` table
- [ ] JSON-aware diffing with dot-notation paths through `Jsonb<T>` nesting
- [ ] Actor attribution via `save_with_actor()` or request-context hook
- [ ] Event logging via `tracing` subscriber layer → event log database

**Deliverable:** Audit trail and observability infrastructure.

---

## Milestone Map

| Phase | Est. Effort | Cumulative Result |
|---|---|---|
| 0: Workspace | Small | Compiling workspace, CI, test DB |
| 1: Core Model | Large | Define structs → CRUD against Postgres |
| 2: Query Builder | Large | Full typed query API |
| 3: Relations | Medium | FK, prefetch, select_related, M2M |
| 4: Txn & Expressions | Large | Transactions, F-expressions, aggregation, bulk upsert |
| **→ SeaORM replacement viable** | | **Phases 0-4 cover all blocking requirements** |
| 5: Postgres Native | Medium | Enums, arrays, JSONB, native aggregates |
| 6: Migrations | Large | Full migration system |
| 7: Hooks & Composition | Medium | Lifecycle hooks, abstract models, proxy, computed properties |
| 8: Shell & Admin | Medium | Interactive tools |
| 9: Logging | Medium | Audit trail |

**The critical path to SeaORM replacement is Phases 0–4.** After Phase 4, the framework can express all 36 entities and their query patterns from the target project. Phases 5–9 add depth and DX improvements that can land incrementally after migration begins.
