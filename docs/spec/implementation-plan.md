> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Djogi Implementation Plan

*Sequenced to reach production-readiness for a real-world Postgres-backed application built on any Rust web framework, with Axum used as the best-covered example today. The target shape includes non-trivial schema breadth, complex queries, PostGIS, and a strong fit alongside popular Rust ORM alternatives for Postgres-heavy applications.*

---

## Guiding Principles

1. **Each phase produces a usable, testable crate** — not a waterfall of unshippable code
2. **The model macro system (`djogi-macros`) is the foundation** — descriptor generation and model metadata land before higher-level APIs
3. **Raw SQL escape hatch ships in Phase 1** — the framework must never trap the developer
4. **Tests against real Postgres** — no mocking the database; use `sqlx::test` fixtures
5. **Postgres-only from day one** — every SQL string targets Postgres directly
6. **Efficient Postgres forms belong in-framework for common work** — raw SQL is for unusual SQL shape, not for recovering performance lost to the ORM

### Idiomatic Rust Guardrails

Djogi should feel like a strong Rust data layer, not a framework that swallows the whole application. The roadmap should be implemented with the following constraints:

- **Public API is type-first, not descriptor-first** — model authors primarily work with normal Rust types, wrappers, and derives; internal descriptor enums and metadata exist to drive SQL generation, migration diffing, and tooling
- **Explicit over magical** — eager loading, transactions, lock behavior, projection boundaries, and escape hatches stay visible at the call site; avoid hidden I/O or implicit behavior shifts
- **Core stays narrow** — the `djogi` crate owns Postgres-native model/query/write/runtime primitives; web-framework integration, admin surfaces, shell conveniences, and app policy layers remain opt-in and clearly layered
- **Feature flags are real boundaries** — optional surfaces should not leak heavyweight dependencies or framework assumptions into the core data path
- **Context objects stay disciplined** — `DjogiContext` may carry execution state needed for correctness, but it must not become a catch-all service locator for unrelated framework concerns
- **Prefer typed wrappers over stringly configuration** — use types like `Jsonb<T>`, `Tracked<T>`, `ForeignKey<T>`, and future validated field types where they add safety; avoid replacing Rust types with piles of string-based annotations

### Performance-Safe Workload Check

Major roadmap items should be evaluated not only as isolated features, but against recurring workload families:

- high-volume feed reads
- concurrent engagement or accounting writes
- job and queue claim flows
- multi-tenant SaaS isolation
- audit and outbox-heavy systems

Djogi does not need product-specific abstractions for any of those domains. It does need the reusable query, write, locking, indexing, and observability primitives that keep those workloads efficient in-framework.

### Execution Strategy

The roadmap below is still phase-ordered, but implementation should run in parallel workstreams with explicit merge points:

- **Workstream A: runtime core (`djogi`)** — `Model` trait, descriptors, field metadata, error types, connection abstractions
- **Workstream B: macro expansion (`djogi-macros`)** — parse model attributes, emit descriptor metadata, generate model impls
- **Workstream C: SQL/runtime behavior** — CRUD SQL builders, raw SQL escape hatch, transaction-compatible execution
- **Workstream D: verification** — compile tests first, then `sqlx::test` integration coverage against real Postgres
- **Workstream E: docs/decisions** — capture architectural constraints early so later phases are not built on invalid assumptions

Every workstream should review new public API against the idiomatic-Rust guardrails above. If a proposal is easier to describe in descriptor jargon than in ordinary Rust types and methods, it is probably landing at the wrong layer.

### Phase 0a: Architecture Checkpoint

**Goal:** Lock the macro shape before Phase 1 code lands.

- [ ] Resolve the Rust macro constraint: `#[derive(Model)]` cannot inject real struct fields into the user-declared type
- [ ] Choose one Phase 1 shape and document it before implementation:
  - [ ] `#[model(...)]` attribute macro owns struct rewriting and field injection, with `#[derive(Model)]` reserved for trait generation only
  - [ ] Or Phase 1 requires explicit framework fields in user structs, with injection deferred
- [ ] Freeze the minimal `ModelDescriptor` format used by both runtime and future migration diffing
- [ ] Define the first shippable slice as "single-table model, default primary key, basic CRUD, raw SQL, real Postgres tests"

**Deliverable:** Macro architecture decision recorded, with Phase 1 narrowed to an implementable slice.

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
- [ ] Implement the Phase 0a macro decision:
  - [ ] If using an attribute macro, inject framework fields: `id: HeerId`, `created_at: OffsetDateTime`, `updated_at: OffsetDateTime`
  - [ ] If staying derive-only for the first slice, validate/preserve explicit framework fields and generate metadata without struct rewriting
- [ ] Generate `impl Model for T` with: `table_name()`, `descriptor()`
- [ ] Generate `impl sqlx::FromRow for T`
- [ ] Generate `ModelDescriptor` and register via `inventory::submit!`
- [ ] Minimal Phase 1 slice: support the default HeerId primary key path first
- [ ] Defer `#[model(pk = "serial")]`, `#[model(pk = "ranjid")]`, `#[model(pk = "none")]`, and composite keys until the default path is stable

### 1b: Field Types

- [ ] Minimal Phase 1 slice:
  - [ ] `String` → `TEXT`
  - [ ] `i32` / `i64` → `INTEGER` / `BIGINT`
  - [ ] `bool` → `BOOLEAN`
  - [ ] `OffsetDateTime` → `TIMESTAMPTZ`
  - [ ] `Option<T>` → nullable
  - [ ] `HeerId` → `BIGINT`
- [ ] Defer `i16`, floats, dates, decimals, UUID/RanjId, JSONB, arrays, and advanced wrappers until CRUD is stable

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

**Phase 1.5 amendment (2026-04-16):** Phase 1 canonicalizes `sqlx::Executor<Database = Postgres>` as the connection abstraction for all CRUD operations. Later phases may introduce a thin Djogi-owned adapter (e.g. `DjogiContext`, planned in Phase 4) if transaction-context policies or pool safety guards require more structure — but such an adapter must extend, not replace, the Phase 1 `Executor` contract. No `DjogiConnection` type exists in Phase 1.

**Phase 4 amendment (2026-04-19):** Per Phase 4 v3's Q1 resolution, the Phase 1 `sqlx::Executor` contract was **replaced** (not extended) by `&mut DjogiContext`. `DjogiContext` carries either a pool or an active transaction and pattern-matches on the inner variant at each sqlx boundary. Every Phase 1/2/3 CRUD / QuerySet / relation signature retrofitted accordingly; the retrofit shipped as a single concentrated commit on `phase4-retrofit`.

### Phase 1 Parallel Tracks

- [ ] **Track A: metadata path** — `ModelDescriptor`, field definitions, `inventory` registration, compile-time tests
- [ ] **Track B: runtime path** — `Model` trait, CRUD SQL generation, raw SQL helpers
- [ ] **Track C: macro path** — attribute parsing, generated impls, `FromRow`, descriptor emission
- [ ] Merge point: one end-to-end model compiles and passes CRUD tests against Postgres

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

Phase 4 is the main query/write power inflection point. It is where Djogi stops being "typed CRUD plus a builder" and starts owning the concurrency and expression substrate needed for serious production workloads.

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

**Deliverable:** Full transaction support, field expressions, aggregation, subqueries, bulk upsert, and the core lock-aware/write-efficient substrate expected of a production Postgres ORM.

---

## Phase 4.5: Projections & Shared Contracts

**Goal:** Generate audience-specific transport-safe types from one model definition.

This phase owns transport-safe contract generation. It does not replace Phase 4's query-time typed result shaping for aggregates, annotations, or other performance-sensitive query outputs.

Projection generation should stay Rust-native: plain data structs, explicit conversions, and no hidden runtime dependency on SQLx, `DjogiContext`, or request-state machinery.

- [ ] Add field exposure metadata for named projection scopes
- [ ] Generate projection structs such as public/self/admin/export views
- [ ] Generate `From<&Model>` conversions into projections
- [ ] Support nested relation projections without exposing raw persistence models
- [ ] Validate projections at compile time when disallowed fields are referenced
- [ ] Keep generated projection types free of SQLx/runtime dependencies so they can live in shared API/frontend crates

**Deliverable:** Models can derive transport-safe projections without handwritten DTO mapping layers.

---

## Phase 5: Postgres-Native Features

**Goal:** First-class support for Postgres features that other ORMs treat as optional.

Phase 5 takes the Phase 4 substrate and makes Postgres-native performance features first-class: typed JSONB and arrays, native aggregates, RLS hooks, index metadata, and typed field families whose schema intent must survive migration/runtime behavior.

This phase is also where Djogi should prefer typed value wrappers and closed internal enums over schema-DSL sprawl. Descriptor richness belongs behind the scenes; model declarations should still read like idiomatic Rust.

### 5a: Postgres Enum Types

- [ ] `#[derive(DjogiEnum)]` on Rust enums → Postgres `CREATE TYPE ... AS ENUM`
- [ ] Auto-map between Rust enum variants and Postgres enum values
- [ ] Support string-backed enums (`#[djogi_enum(as_string)]`) for schema evolution flexibility
- [ ] Migration support: `ALTER TYPE ... ADD VALUE`

### 5b: Typed String Field Primitives

- [ ] Distinguish bounded character fields from unbounded text fields in descriptor metadata and migration diffs
- [ ] Preserve `VARCHAR(n)` versus `TEXT` as an explicit schema choice rather than collapsing both into "String + validator"
- [ ] Keep the Rust-side API ergonomic, but ensure `TEXT <-> VARCHAR(n)` is treated as a real alteration by the differ
- [ ] Let validated field families such as email/phone/locale build on top of these primitives rather than re-defining string storage ad hoc

### 5c: Postgres Array Fields

- [ ] `Vec<String>` → `TEXT[]`, `Vec<i32>` → `INTEGER[]`, etc.
- [ ] Array lookups: `contains` (`@>`), `contained_by` (`<@`), `overlap` (`&&`)
- [ ] Array length: `.len()` lookup
- [ ] Array index access in expressions: `field[0]`

### 5d: Typed JSONB (`Jsonb<T>`)

- [ ] `Jsonb<T>` with typed schema + unknown field preservation (per existing spec)
- [ ] JSONB path lookups: `has_key` (`?`), `has_keys` (`?&`), `has_any_keys` (`?|`)
- [ ] JSONB containment: `contains` (`@>`), `contained_by` (`<@`)
- [ ] Subfield query filters via proc macro (per existing spec)
- [ ] Validation on save with dot-notation error paths

### 5e: Postgres-Native Aggregates

- [ ] `ArrayAgg(field)` → `ARRAY_AGG()` with ordering and distinct
- [ ] `JsonAgg(field)` → `JSONB_AGG()`
- [ ] `StringAgg(field, delimiter)` → `STRING_AGG()`
- [ ] `BoolAnd` / `BoolOr` → `BOOL_AND()` / `BOOL_OR()`

### 5f: Postgres-Native Indexes

- [ ] `#[index(gin)]` / `#[index(gist)]` / `#[index(brin)]` — index type annotations
- [ ] `OpClass` support for trigram and JSONB indexing
- [ ] Partial indexes: `#[index(condition = "active = true")]`
- [ ] Covering indexes: `#[index(include = ["field"])]`

### 5g: Database Functions

- [ ] Comparison: `Coalesce`, `Greatest`, `Least`, `NullIf`, `Cast`
- [ ] Text: `Lower`, `Upper`, `Trim`, `Concat`, `Replace`, `Substr`, `Length`
- [ ] DateTime: `Now`, `Extract`, `TruncDate`
- [ ] Math: `Abs`, `Ceil`, `Floor`, `Round`

### 5h: Streaming / Cursor Terminals

- [ ] `QuerySet::stream(&mut ctx)` returning an `impl Stream<Item = Result<T>>` backed by a Postgres named cursor
- [ ] Cursor lifecycle pinned to an active `atomic()` scope (cursors are transaction-local in Postgres)
- [ ] Configurable fetch-size window (default 1000 rows per `FETCH`)
- [ ] Backpressure via `Stream` polling; never buffer the full result set in memory
- [ ] Escape-hatch `ctx.raw_stream(sql, binds)` for streaming raw queries

### 5i: Full-Text Search

- [ ] `tsvector` field type (`TsVector`) + `tsquery` predicate type (`TsQuery`)
- [ ] Model-level `#[model(fts = { source = "title, body", dictionary = "english" })]` generates a `GENERATED ALWAYS AS` column + GIN index
- [ ] Query-site `.filter(|m| m.search.matches(query("planet earth")))` produces `@@` match predicates
- [ ] `ts_rank` / `ts_rank_cd` as aggregate helpers for ranking result ordering
- [ ] Dictionary choice surfaced in migration diffs (so dictionary changes show up as an alteration)

**Deliverable:** Postgres enums, explicit string field primitives, arrays, typed JSONB, native aggregates, indexes, database functions, streaming terminals, full-text search.

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

### 6f: Online / Zero-Downtime Migration Patterns

- [ ] Advisory-lock-based single-active-migration coordination (no two `cargo djogi migrate` invocations apply concurrently against the same database)
- [ ] Lock-timeout on DDL statements so blocked migrations back off rather than queue behind long transactions (`SET lock_timeout = '5s'` around each DDL)
- [ ] Two-phase column rename: emit `ADD COLUMN new_name` + backfill from `old_name` + runtime reads both + drop `old_name` in a follow-up migration. Driven by `#[field(renamed_from = "old_name")]` + an opt-in `#[field(rename_strategy = "two_phase")]`
- [ ] Two-phase type widening: add new-type column, backfill, cut over, drop old (analogous pattern)
- [ ] Safe NOT NULL addition: `ADD COLUMN ... DEFAULT value` (Postgres 11+ makes this fast-path without table rewrite) plus a `VALIDATE` pass for pre-existing-table columns
- [ ] Constraint addition with `NOT VALID` + `VALIDATE` as separate steps
- [ ] Backfill orchestration primitive: chunked `UPDATE ... WHERE pk BETWEEN $1 AND $2` with configurable chunk size, delay between chunks, progress reporting
- [ ] Destructive-op detection: dropping a column, dropping a table, narrowing a type — gated behind `--allow-destructive` (already in 6a) with an additional "migration is not online" warning emitted at generation time

**Deliverable:** Full migration system with drift detection, SQL generation, CLI, data migrations, online migration patterns.

---

## Phase 6.5: Protected Data Metadata & Field Codecs

**Goal:** Add descriptor-level protected-field semantics and storage transforms.

Protected-data support should extend the typed field story rather than replace it with policy soup. Sensitive-field metadata, codecs, and redaction rules belong in descriptor/tooling layers, while ordinary app code should continue to interact with clear Rust types.

- [ ] Add field metadata for sensitivity, redaction scope, rationale, and lifecycle class
- [ ] Add descriptor support for field codecs such as encrypted/tokenized/custom-serialized columns
- [ ] Ensure CRUD generation and row decoding apply codecs consistently
- [ ] Integrate protected-field metadata with generated projections and admin defaults
- [ ] Emit compile-time diagnostics when sensitive annotations are underspecified

**Deliverable:** Djogi can express protected-field intent once and apply it consistently across generated surfaces.

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

These surfaces are intentionally downstream of the core ORM/runtime. They are useful operational tools, but they must remain feature-gated adapters over the model/query layer rather than redefining the core identity of the crate.

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

Admin integration may be Axum-oriented in practice, but that coupling should live in the feature-gated admin layer. Core model/query/runtime APIs should not require Axum types or assumptions.

### 8c: Static Query Analyzer

- [ ] `cargo djogi analyze query` — walks every crate in the workspace, parses `.rs` files with `syn`, finds every QuerySet terminal (`.fetch_all`, `.fetch_one`, `.first`, `.exists`, `.count`, `.delete`, `.update`, `.stream`) and every `raw_query` / `execute_raw` call site
- [ ] N+1 detector: flag any terminal whose AST ancestor chain includes a `for` / `while` / iterator `.map` / `.for_each` — these are the shape of the classic N+1. Suggestion message names the FK and points at the `.prefetch()` call that would replace it
- [ ] Graph-aware repeat-node detection: the descriptor registry is already a directed graph (tables as nodes, FKs as edges). Within a scope (function body, `async` block, `atomic()` closure), the analyzer tracks the set of `(model, filter_fingerprint)` pairs reached by terminals. Repeat visits to the same node — whether from independent call sites, through different FK traversals, or across prefetch chains that partially overlap — are flagged with a suggestion to hoist the fetch to an outer scope, fold the filters into a single `WHERE`, or cover both accesses with a unified `select_related` / `prefetch_related` chain. Goes beyond loop-shape N+1: catches the case where two unrelated code paths in the same request both fetch the same parent row
- [ ] Over-fetching detector: when a QuerySet hydrates a full `Model` but the receiving scope only reads a known-small subset of fields, suggest the matching projection type (declared via `#[model(expose(...))]`) or a new `expose` group
- [ ] `.fetch()` vs `.prefetch()` misuse: when `.fetch()` appears inside an iterator over a parent collection whose FK is declared, point at `.prefetch()` + the exact `Related` accessor to use instead
- [ ] Output modes: `--format human` (colorized, grouped by file), `--format json` (machine-readable for editor integration), `--format clippy` (compatible with `cargo clippy --message-format json`)
- [ ] Severity gating: `--deny <lint>` turns a warning into a non-zero exit code for CI
- [ ] Scope: pure static analysis — does not require a database connection, does not run queries, does not load `target/djogi_models.json` beyond what's needed to resolve FK topology

**Deliverable:** Working shell, admin panel, and `cargo djogi analyze query` lint pass.

---

## Phase 8.5: Data Lifecycle & Governance

**Goal:** Turn lifecycle metadata into reviewable operator workflows.

- [ ] Add model/field lifecycle classes for purge, anonymize, archive, and permanent retention
- [ ] Generate dependency-aware lifecycle plans from model descriptors
- [ ] Add legal-hold primitives that override generated lifecycle plans
- [ ] Expose CLI planning/review/apply workflows for lifecycle operations
- [ ] Ensure lifecycle operations emit audit and event records

**Deliverable:** Djogi can plan and execute safe data-lifecycle operations without embedding product workflow logic in app code.

---

## Phase 9: CRUD Logging & Observability

**Goal:** Automated audit trail plus concrete observability hooks (tracing, metrics, slow-query callbacks) that apps can integrate with standard Rust observability crates.

### 9a: Audit Trail

- [ ] Three-database architecture: app, crud_logs, event_logs (pools already defined in Phase 0/1)
- [ ] Per-model `#[model(crud_log = true)]` — auto-provision mirror `_logs` table
- [ ] JSON-aware diffing with dot-notation paths through `Jsonb<T>` nesting
- [ ] Actor attribution via `save_with_actor()` or request-context hook

### 9b: Tracing Integration

- [ ] Emit a `tracing::Span` per query with fields: `sql_text` (truncated, no bind values), `duration_ms`, `rows_affected`, `pool_wait_ms`, `model_name` (when derivable)
- [ ] Span attachment to surrounding `atomic()` scope's span (so transactions appear as parent spans over their queries)
- [ ] Opt-out per model via `#[model(trace = false)]` for hot-path tables

### 9c: Slow-Query Callbacks

- [ ] `djogi::observe::register_slow_query_handler(threshold: Duration, handler: impl Fn(&QueryTelemetry))`
- [ ] `QueryTelemetry` carries: sql, duration, row count, backend pid, lock wait time, which connection pool
- [ ] Guaranteed called after query completion (success or error); handler runs on the query task's executor

### 9d: Metrics Emission

- [ ] `metrics` crate integration: histograms for query duration, counters for rows affected, gauges for pool utilization + idle vs active connections
- [ ] Per-model breakdown labels (opt-in via `#[model(metrics = true)]`)
- [ ] Pool-level metrics per the three-pool architecture

### 9e: Admin-UI Observability Views

- [ ] Phase 8's admin layer surfaces slow-query log, pool stats, long-running transactions, recent `crud_logs` entries for a given record — provided the observability hooks from 9b/9c/9d are wired
- [ ] Zero additional cost when the admin feature isn't enabled; the hooks stand alone
- [ ] Per-request debug drawer (gated on `dev_mode = true` + `admin` feature flag): bottom panel on every `/_admin/` page showing queries issued during the request, per-query duration, originating `tracing` span, rows returned, and a SQL-text preview with binds inlined for readability
- [ ] Click-to-EXPLAIN: each drawer row exposes an "Explain" action that runs `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` against a savepoint so production side effects aren't re-executed; the plan is rendered as a collapsible tree with per-node cost and row-count estimates
- [ ] Semantic N+1 flag: because Djogi knows the FK topology at compile time, the drawer annotates any relation fetched more than K times within a single request span with the exact model + FK name and the `.prefetch()` call that would collapse it — no pattern-matching heuristics, the detection is driven by declared structure
- [ ] Optional middleware hook (shipped under each web-framework sub-feature flag — `axum`, `warp`, etc.) that injects the drawer into any HTML response in dev mode, not just admin pages; API-only apps get the same data as a `X-Djogi-Queries` response header + a debug JSON endpoint

### 9f: Event Logging

- [ ] Event logging via `tracing` subscriber layer writing to the event log database
- [ ] Schema for events: timestamp, level, target, fields, parent span id
- [ ] Retention policy opt-in (delete events older than N days)

**Deliverable:** Audit trail + tracing spans + slow-query hooks + metrics + admin dashboards + event logging.

---

## Phase 9.5: Operational Tooling

**Goal:** Turnkey solution for the boring-but-critical operational work every Postgres app needs — backups, vacuums, maintenance schedules, disaster recovery drills. Without this, teams hand-roll it inconsistently and find out in production it was wrong.

### 9.5a: Scheduled Backups

- [ ] `cargo djogi ops backup setup --daily [--weekly] [--retention 14d]` — generates a platform-appropriate scheduler config (cron fragment, systemd timer unit, or launchd plist) + a backup script that wraps `pg_dump --format=custom` with sane defaults (parallelism, compression)
- [ ] `cargo djogi ops backup now` — one-shot manual backup
- [ ] `cargo djogi ops backup verify <file>` — runs `pg_restore --list` to confirm the archive is restorable
- [ ] Storage targets: local path, S3-compatible (via env-var-configured endpoint + credentials), optional `rclone` passthrough
- [ ] Retention policy enforcement (prune backups older than configured retention)

### 9.5b: Point-In-Time Recovery (opt-in)

- [ ] `cargo djogi ops pitr setup` — configures WAL archiving to a specified target, generates `restore.conf` template
- [ ] `cargo djogi ops pitr restore --target-time '...'` — restore drill runbook that produces a new database at a specific wall-clock time

### 9.5c: Vacuum / Maintenance Scheduling

- [ ] Per-model autovacuum tuning: `#[model(autovacuum = VacuumPolicy::HighChurn)]` emits per-table `ALTER TABLE ... SET (autovacuum_vacuum_scale_factor = ..., ...)` in migration SQL
- [ ] `cargo djogi ops vacuum --table <name> [--analyze] [--full]` — on-demand vacuum/analyze
- [ ] `cargo djogi ops vacuum setup --weekly` — scheduled `VACUUM ANALYZE` across the schema, respecting autovacuum settings

### 9.5d: Health Checks

- [ ] `cargo djogi ops doctor` — checks pool utilization, long-running transactions (> N seconds), table bloat estimates, index bloat, replication lag if configured, `pg_stat_statements` top-N slow queries
- [ ] Each check returns a pass/warn/fail with a suggested remediation

### 9.5e: Operator Runbooks

- [ ] Generate opinionated Markdown runbooks under `docs/ops/` covering: "my backup failed", "restore from last night", "I accidentally dropped a table", "vacuum is blocked"
- [ ] Runbooks reference the specific `cargo djogi ops` commands that resolve each scenario

**Deliverable:** Djogi apps get production-grade ops (backups, PITR, vacuum, health, runbooks) without cobbling them together per project.

---

## Phase 10: Distributed Topology & Residency

**Goal:** Add descriptor-aware support for replicas, residency constraints, and topology-sensitive migration safety.

- [ ] Add explicit read-consistency modes such as primary-only, replica-allowed, read-your-writes, and stale-ok
- [ ] Add placement metadata for shard keys, residency classes, and relation placement constraints
- [ ] Validate topology-sensitive schema changes in migration tooling
- [ ] Extend repartition/partition tooling with topology-aware safety checks
- [ ] Keep deployment-specific routing implementations outside Djogi core

**Deliverable:** Djogi remains deployment-agnostic while providing the metadata, runtime contracts, and migration guardrails needed for distributed Postgres topologies.

---

## Milestone Map

| Phase | Est. Effort | Cumulative Result |
|---|---|---|
| 0: Workspace | Small | Compiling workspace, CI, test DB |
| 1: Core Model | Large | Define structs → CRUD against Postgres |
| 2: Query Builder | Large | Full typed query API |
| 3: Relations | Medium | FK, prefetch, select_related, M2M |
| 4: Txn & Expressions | Large | Transactions, F-expressions, aggregation, bulk upsert |
| 4.5: Projections | Medium | Shared transport-safe contracts derived from models |
| **→ Strong option among Rust ORM alternatives for write-heavy Postgres services** | | **Phases 0-4 cover the blocking transaction, expression, and bulk-write substrate** |
| 5: Postgres Native | Medium | Enums, arrays, JSONB, native aggregates, streaming terminals, full-text search |
| 6: Migrations | Large | Full migration system including online / zero-downtime patterns |
| 6.5: Protected Data | Medium | Sensitive-field metadata and codecs |
| 7: Hooks & Composition | Medium | Lifecycle hooks, abstract models, proxy, computed properties |
| 8: Shell & Admin | Medium | Interactive tools |
| 8.5: Lifecycle | Medium | Governance and lifecycle planning (depends on 6.5) |
| 9: Logging & Observability | Medium | Audit trail, tracing, slow-query hooks, metrics, admin views |
| 9.5: Ops Tooling | Medium | Turnkey backups, PITR, vacuum scheduling, health checks, runbooks |
| 10: Topology | Large | Residency, replica semantics, distributed guardrails |

**The critical path to standing alongside popular Rust ORM alternatives is Phases 0–4.** Phase 4.5 improves contract hygiene and shared contract reuse without changing that write-path boundary. Phases 5–10 add the Postgres-native depth, governance, and scale-oriented capabilities needed for broader high-scale confidence.
