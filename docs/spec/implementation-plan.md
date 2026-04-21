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
- **Explicit over magical** — eager loading, transactions, lock behavior, visage boundaries, and escape hatches stay visible at the call site; avoid hidden I/O or implicit behavior shifts
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

## Phase 4.5: Visages & Shared Contracts

**Goal:** Generate audience-specific transport-safe types from one model definition.

This phase owns transport-safe contract generation. It does not replace Phase 4's query-time typed result shaping for aggregates, annotations, or other performance-sensitive query outputs.

Visage generation should stay Rust-native: plain data structs, explicit conversions, and no hidden runtime dependency on SQLx, `DjogiContext`, or request-state machinery.

- [ ] Add field exposure metadata for named visage scopes
- [ ] Generate visage structs such as public/self/admin/export views
- [ ] Generate `From<&Model>` conversions into visages
- [ ] Support nested relation visages without exposing raw persistence models
- [ ] Validate visages at compile time when disallowed fields are referenced
- [ ] Keep generated visage types free of SQLx/runtime dependencies so they can live in shared API/frontend crates

**Deliverable:** Models can derive transport-safe visages without handwritten DTO mapping layers.

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

### 5j: Visage Query Surface + Boundary Enforcement

Phase 4.5 shipped visages as output-shape types only: `{Model}Public` / `SelfView` / `Admin` / `Export` carry `impl TryFrom<&Model>` for conversion but have no query-side surface. Phase 5 §5j makes visages first-class query entities with compile-time scope enforcement, filling Phase 4.5's explicit M2M visage deferral in the process.

- [ ] Visage as query entry point: `PublicRegisteredOwner::filter(|o| o.display_name.eq("Ada")).fetch_all(&mut ctx).await?` — every visage type gets the full QuerySet surface (filter, order, limit, fetch terminals) that models already have
- [ ] Per-visage generated field-accessor types: alongside `{Model}Fields`, each visage emits `{Visage}Fields` surfacing only the fields the visage exposes. Attempting to reference a non-exposed field in a closure is a compile error, not a runtime omission
- [ ] Forward-FK boundary enforcement: relation fields in the visage's accessor type point to visage-scoped peer accessors. `PublicRegisteredOwner::filter(|o| o.address.city.eq("Toronto"))` compiles; `.address.street` does not compile when `AddressPublic` exposes only `.city`. Enforcement is compile-time via the type system — zero runtime cost
- [ ] Reverse-FK boundary enforcement: symmetrical. From `AddressPublic`, the reverse accessor to owners returns `QuerySet<PublicRegisteredOwner>` where the closure receiver is `PublicRegisteredOwnerFields`
- [ ] M2M boundary enforcement: visage-scoped M2M accessor methods return visage-scoped QuerySets. Through-model fields can opt into their own visage via `#[field(expose(...))]` on through-model fields, producing e.g. `UserInterestPublic`. Through-model visages participate in the same boundary enforcement. Fills the Phase 4.5 deferral "M2M visages — visages nest only through `ForeignKey<T>` / `OneToOneField<T>`; M2M stitching is manual"
- [ ] SELECT narrowing: visage-scoped QuerySets emit SELECT with only the visage's exposed columns, not the full model. This is the headline performance win — visages stop being only an output shape and start paying off at the query side
- [ ] Mutation scope: visage-scoped queries are read-only by default. `save` / `delete` / `update_or_create` on a visage emit a compile error pointing at the source model. Kept simple in v1; revisit only if a clear use case arrives
- [ ] Prefetch composition: an API for declaring a prefetch into a specific peer visage so chained traversals inherit the same boundary. Exact shape deferred to v2/v3 spec; leading candidates include a dedicated `prefetch_as::<PeerProjection>(model::relation)` terminal on the QuerySet and a generated `model::relation::as_public()`-style relation-path variant surfaced under each visage scope
- [ ] Interaction with §8c: Phase 8c's Tier 1 over-fetching detector gains a concrete suggestable fix — "you hydrated `RegisteredOwner` but only read `.display_name` + `.email` — swap for `PublicRegisteredOwner`"

**Deliverable:** Postgres enums, explicit string field primitives, arrays, typed JSONB, native aggregates, indexes, database functions, streaming terminals, full-text search, visage query surface with compile-time FK / reverse-FK / M2M boundary enforcement.

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

- [ ] Phased migration execution model: the migration runner splits each generated migration into ordered step groups tagged transactional vs non-transactional. Transactional groups run inside `BEGIN/COMMIT`; non-transactional steps — `CREATE INDEX CONCURRENTLY`, `DROP INDEX CONCURRENTLY`, certain `CREATE EXTENSION` cases, some `ALTER TYPE ADD VALUE` operations — run outside any transaction. `atomic()` is available only around transactional steps; attempting to wrap a non-transactional step in `atomic()` produces a clear error ("this step cannot run inside a transaction — see Phase 6f phased-migration model") rather than a silent SQLSTATE from Postgres
- [ ] Advisory-lock-based single-active-migration coordination (no two `cargo djogi migrate` invocations apply concurrently against the same database)
- [ ] Lock-timeout on DDL statements so blocked migrations back off rather than queue behind long transactions (`SET lock_timeout = '5s'` around each DDL)
- [ ] Two-phase column rename: emit `ADD COLUMN new_name` + backfill from `old_name` + runtime reads both + drop `old_name` in a follow-up migration. Driven by `#[field(renamed_from = "old_name")]` + an opt-in `#[field(rename_strategy = "two_phase")]`
- [ ] Two-phase type widening: add new-type column, backfill, cut over, drop old (analogous pattern)
- [ ] Safe NOT NULL addition: `ADD COLUMN ... DEFAULT value` (Postgres 11+ makes this fast-path without table rewrite) plus a `VALIDATE` pass for pre-existing-table columns
- [ ] Constraint addition with `NOT VALID` + `VALIDATE` as separate steps
- [ ] Backfill orchestration primitive: chunked `UPDATE ... WHERE pk BETWEEN $1 AND $2` with configurable chunk size, delay between chunks, progress reporting
- [ ] Destructive-op detection: dropping a column, dropping a table, narrowing a type — gated behind `--allow-destructive` (already in 6a) with an additional "migration is not online" warning emitted at generation time
- [ ] Backfill side-effect suppression: chunked `UPDATE` backfills run with outbox emission and audit writes suppressed by default. Migrations represent schema evolution, not domain events; firing outbox messages and audit rows for every historical row rewritten during a backfill is never the right default. Opt in per migration via an explicit `emit_side_effects = true` flag when the backfill genuinely is a business event

**Deliverable:** Full migration system with drift detection, SQL generation, CLI, data migrations, online migration patterns.

---

## Phase 6.5: Protected Data Metadata & Field Codecs

**Goal:** Add descriptor-level protected-field semantics and storage transforms.

Protected-data support should extend the typed field story rather than replace it with policy soup. Sensitive-field metadata, codecs, and redaction rules belong in descriptor/tooling layers, while ordinary app code should continue to interact with clear Rust types.

- [ ] Add field metadata for sensitivity, redaction scope, rationale, and lifecycle class
- [ ] Add descriptor support for field codecs such as encrypted/tokenized/custom-serialized columns
- [ ] Ensure CRUD generation and row decoding apply codecs consistently
- [ ] Integrate protected-field metadata with generated visages and admin defaults
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
- [ ] **djqry authoring loop** — the shell is the primary surface for iterating on `djqry` overrides (§8d). Workflow: *test → optimize → compile → deploy*. Shell commands:
  - `djqry.export(<last_query>, "<name>")` — writes `djqry/<name>.sql` with frontmatter pre-populated from the last executed macro-query: `@name` set, `@on` inferred from the query's target models, `@replaces` captured verbatim, `@signature` computed, `@returns` inferred from the QuerySet's declared return type, `@binds` inferred from the filter closures, and the macro-generated SQL placed in the body as the starting point the author can optimize against
  - `djqry.import("<name>")` — loads an existing `djqry/<name>.sql`, parses its frontmatter + SQL, binds the override into the shell session as a callable, and runs it alongside the macro-query form for side-by-side comparison (row count, first-row diff, timing)
  - `djqry.diff("<name>")` — runs macro-query and override both, reports result-set diff + `EXPLAIN` cost comparison + timing. Acts as the local on-demand analog of CI's `cargo djogi djqry verify`
  - `djqry.sign("<name>")` — re-computes the fingerprint from the current `@replaces` and updates `@signature`, asserting the author has re-verified. Prompts for confirmation before overwriting

### 8b: Admin Panel (HTMX + Askama)

- [ ] Auto-generate list view from `ModelDescriptor` with pagination, sorting, search
- [ ] Auto-generate CRUD forms from field metadata
- [ ] HTMX-driven: partial page updates for pagination, filtering, inline editing
- [ ] `admin` feature flag — opt-in dependency
- [ ] Annotation-driven customization: `#[admin(list_display = [...])]`
- [ ] Trait-based advanced customization: `impl AdminConfig for T`

Admin integration may be Axum-oriented in practice, but that coupling should live in the feature-gated admin layer. Core model/query/runtime APIs should not require Axum types or assumptions.

### 8c: Static Query Analyzer

The analyzer ships as two tiers with different fidelity guarantees. Tier 1 is mainline and intended for CI gating by default. Tier 2 is experimental and best-effort — surfaced as warnings, never as `--deny` targets unless explicitly requested.

**Data sources.** Call-site discovery comes from source AST via `syn`. Model metadata (FK topology, visage maps, field descriptors) comes from `target/djogi_models.json`, which is emitted by the existing `#[model]` + `build.rs` pipeline during a normal `cargo build`. The analyzer requires a successful build to run — the metadata file is the FK graph's authoritative source, not a guess inferred from AST.

- [ ] `cargo djogi analyze query` — walks every crate in the workspace, parses `.rs` files with `syn`, finds every QuerySet terminal (`.fetch_all`, `.fetch_one`, `.first`, `.exists`, `.count`, `.delete`, `.update`, `.stream`) and every `raw_query` / `execute_raw` call site

**Tier 1 — mainline, high-signal, low-false-positive (syn + metadata file, no type resolution needed):**

- [ ] Loop-shape N+1 detector: flag any terminal whose AST ancestor chain includes a `for` / `while` / iterator `.map` / `.for_each` closure. Receiver-type resolution is best-effort; when the receiver is unambiguous (e.g., `User::filter().fetch_all()`), the suggestion message names the FK and points at the `.prefetch()` call that would replace it. When the receiver is generic or goes through a helper, the lint still fires but with a softer message
- [ ] `.fetch()` vs `.prefetch()` misuse: when `.fetch()` appears inside an iterator over a parent collection whose FK is declared, point at `.prefetch()` + the exact `Related` accessor to use instead
- [ ] Over-fetching detector: when a QuerySet hydrates a full `Model` and the same scope only reads a small, enumerable subset of fields on the result, suggest the matching visage type (declared via `#[model(expose(...))]`) or propose a new `expose` group. Conservative: only fires when the post-hydration field access is fully visible in the AST; silent otherwise

**Tier 2 — experimental, opt-in, best-effort graph-aware analysis:**

- [ ] Graph-aware repeat-node detection: the descriptor registry's FK topology (from `target/djogi_models.json`) is a directed graph of tables-as-nodes and FKs-as-edges. Within a scope (function body, `async` block, `atomic()` closure), the analyzer attempts to track the set of `(model, filter_fingerprint)` pairs reached by terminals. Where receiver types resolve cleanly via `syn`, repeat visits to the same node — whether from independent call sites, through different FK traversals, or across prefetch chains that partially overlap — are flagged with a suggestion to hoist the fetch, fold the filters, or cover both accesses with a unified `select_related` / `prefetch_related` chain
- [ ] Honest caveat: `syn` alone cannot fully resolve receiver types through generic wrappers, re-exports, or helper indirection. When the analyzer cannot resolve a receiver, it silently skips rather than guessing. Coverage is documented as "high-signal when receiver is unambiguous; silent otherwise". A future upgrade path — rustc/HIR or `rust-analyzer`-as-a-library — is named in the follow-up list but not a Phase 8c deliverable

**Output + gating:**

- [ ] Output modes: `--format human` (colorized, grouped by file), `--format json` (machine-readable for editor integration), `--format clippy` (compatible with `cargo clippy --message-format json`)
- [ ] Severity gating: `--deny <lint>` turns a Tier 1 warning into a non-zero exit code for CI. Tier 2 lints default to warn-only; `--deny experimental` is an explicit opt-in for teams willing to accept Tier 2 false-positive risk
- [ ] Scope: pure static analysis beyond what a `cargo build` already produces. No database connection, no query execution. The pre-existing `target/djogi_models.json` build artifact is the only runtime input

### 8d: `djqry` SQL Override Registry

When a multi-hop macro-query compiles to a plan that is significantly worse than a hand-written query, the escape hatch today is `ctx.raw_query::<T>(...)` — which fragments the codebase visually and decouples the site from descriptor-aware tooling (static analyzer, admin surface, observability labels). `djqry` keeps the hand-tuned SQL in its own file while surfacing it as a typed method on the relevant models, preserving the declarative call-site shape elsewhere and giving the override the same type-safety, tracing, and analyzer treatment as macro-generated queries.

- [ ] `djqry/` directory at repo root holds `.sql` files; each file declares one override via frontmatter header comments
- [ ] Frontmatter schema: `@name` (method name, snake_case), `@on` (comma-separated list of models and / or visages; `_global` for non-model-scoped overrides), `@returns` (Rust type implementing `FromPgRow`), `@binds` (positional bind types — `()` for none), `@replaces` (multi-line canonical macro-query the override optimizes — documentation plus drift-check source), `@signature` (fingerprint hash bumped on manual re-verification)
- [ ] Build-time generation: a new stage in the existing `build.rs` pipeline (alongside `target/djogi_models.json` emission) parses every `.sql` file, validates frontmatter against descriptor metadata, and emits a generated `{Model}Djqry` zero-sized type per owner with one associated async function per override. Call site reads `VehicleDjqry::expired_registrations(&mut ctx).await?` — parallel to Phase 2's `{Model}Filter` and Phase 3's `{Model}Related` generated types, which is the established convention for per-model namespaced helpers. The `Djqry` suffix is distinctive, grep-able, and zero collision risk. For `@on: _global` overrides the parallel type is `GlobalDjqry`: `GlobalDjqry::fleet_stats(&mut ctx).await?`
- [ ] Multi-owner: when `@on:` lists several owners, delegating methods are generated on each. All delegates resolve to the same compiled SQL; the graph-aware Tier 2 of §8c uses the `@on:` list to reason about which node-visits the override covers
- [ ] Drift detection — mandatory: the build pipeline re-computes the AST-shape fingerprint of `@replaces` (structure plus types plus FK topology from `target/djogi_models.json`, not filter literals) and fails the build when it diverges from the stored `@signature`. Failure message names the model graph before and after, asks the author to re-verify, and suggests a new signature value to copy
- [ ] Drift detection — opt-in: `cargo djogi djqry verify <name>` runs the macro-query and the override against a live database, diffs result sets, reports. CI gates on this; local builds skip it for speed. Local devs may run it on-demand when bumping a signature
- [ ] Runtime dispatch: each generated method routes through `ctx.raw_query::<T>(...)` (Phase 5 substrate) and decodes via `FromPgRow`. An override-firing tracing event names the override so Phase 9b / 9e observability surfaces highlight hand-tuned queries distinctly from macro-generated ones
- [ ] Error modes flagged at build time: missing required frontmatter field, unknown `@on` owner, `@returns` type missing `FromPgRow`, `@binds` arity mismatch with `$N` placeholder count in SQL, reserved-name collision with framework-generated methods, `@signature` mismatch
- [ ] Scope limits: v1 is read-only (SELECT-shaped overrides). `UPDATE` / `DELETE` / `INSERT` overrides deferred until a concrete use case surfaces — raw `ctx.execute_raw` remains available in the interim
- [ ] Authoring loop lives in the shell (§8a): `djqry.export`, `djqry.import`, `djqry.diff`, `djqry.sign` close the *test → optimize → compile → deploy* cycle inside the REPL. Authoring a new override never requires leaving the shell to hand-craft frontmatter — `export` captures the canonical macro-query, infers `@returns` / `@binds` from the QuerySet's declared types, computes the initial `@signature`, and seeds the SQL body with the macro-generated query as the baseline for optimization

**Deliverable:** Working shell, admin panel, `cargo djogi analyze query` lint pass, and `djqry` SQL override registry surfaced as typed model methods with a shell-native authoring loop.

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
- [ ] Profile-first logging config: `light`, `balanced`, `strict_audit`; advanced per-sink overrides only as escape hatches
- [ ] Per-model `#[model(crud_log = true)]` — auto-provision mirror `_logs` table
- [ ] JSON-aware diffing with dot-notation paths through `Jsonb<T>` nesting
- [ ] Actor attribution via `save_with_actor()` or request-context hook
- [ ] Make CRUD delivery semantics explicit: best-effort, durable bounded retry, or fail-closed depending on profile
- [ ] Surface sink health and degraded mode clearly in metrics / CLI / tracing output
- [ ] Document and enforce that strict audit means rejecting app writes when required CRUD audit cannot be satisfied, not cross-database atomic commit

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
- [ ] Click-to-EXPLAIN: each drawer row exposes an "Explain" action that runs `EXPLAIN (FORMAT JSON)` by default — pure planner inspection, no execution, zero side effects regardless of statement kind. An explicit "Explain with Analyze" opt-in is available for SELECTs only; for INSERT/UPDATE/DELETE the `ANALYZE` variant is disabled in the UI with a visible note that `EXPLAIN ANALYZE` executes the statement and that non-transactional effects (`nextval` advancement, `LISTEN/NOTIFY`, deferred trigger side-channels) are not reclaimed by a wrapping savepoint. Plans render as a collapsible tree with per-node cost and row-count estimates
- [ ] Semantic N+1 flag: because Djogi knows the FK topology at compile time, the drawer annotates any relation fetched more than K times within a single request span with the exact model + FK name and the `.prefetch()` call that would collapse it — no pattern-matching heuristics, the detection is driven by declared structure
- [ ] Dev-only scope: the drawer is feature-flagged out of release builds and has no staging/canary mode. Non-dev environments rely on §9b/9c/9d (tracing spans, slow-query callbacks, metrics) for query visibility. If a team wants drawer-like introspection in staging, that is a separate future item, not a Phase 9e deliverable
- [ ] Optional middleware hook (shipped under each web-framework sub-feature flag — `axum`, `warp`, etc.) that injects the drawer into any HTML response in dev mode, not just admin pages. API-only apps get per-request correlation via a stable request ID — the middleware generates an ID per request and the response carries it in a compact `X-Djogi-Queries` header of the form `id=<token>; count=12; slow=2; total_ms=47`. Full per-query detail is retrieved (dev-mode only) by calling `GET /_djogi/debug/request/<id>`, which looks up the trace in a bounded in-memory ring buffer keyed by ID. This is correlation-safe under HTTP/1.1 keep-alive, HTTP/2 multiplexing, client-side connection pooling, and multi-instance deployments where "most recent on this connection" would be ambiguous or racy. Ring buffer size is configurable with a sensible default (128 entries, oldest-evicted); entries carry the full query list, per-query durations, binds, and the originating tracing span ID

### 9f: Event Logging

- [ ] Event logging via `tracing` subscriber layer writing to the event log database
- [ ] Schema for events: timestamp, level, target, fields, parent span id
- [ ] Retention policy opt-in (delete events older than N days)
- [ ] Keep event logging best-effort in built-in profiles; expose dropped-event counters and sink-failure warnings

### 9g: Log-Database Operations

- [ ] Unified operator workflow for app / CRUD-log / event-log migrations with explicit per-database labeling
- [ ] `db reset` remains app-first; touching logging databases requires explicit flags
- [ ] Startup checks honor profile semantics: `light` tolerates missing sinks, `balanced` starts degraded with warnings, `strict_audit` refuses startup when required CRUD audit sink is unavailable

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

- [ ] Per-model autovacuum tuning: `#[model(autovacuum = VacuumPolicy::HighChurn)]` emits per-table `ALTER TABLE ... SET (autovacuum_vacuum_scale_factor = ..., ...)` as DDL routed through Phase 6's migration generation pipeline. Phase 9.5 provides the policy vocabulary + CLI/ops surface; Phase 6 owns the DDL emission and phased execution
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
| 4.5: Visages | Medium | Shared transport-safe contracts derived from models |
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
