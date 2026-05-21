> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Djogi Implementation Plan

*Sequenced to reach production-readiness for a real-world Postgres-backed application built on any Rust web framework, with Axum used as the best-covered example today. The target shape includes non-trivial schema breadth, complex queries, PostGIS, and a strong fit alongside popular Rust ORM alternatives for Postgres-heavy applications.*

> **Note on accuracy:** This file captures the *original* phased
> plan and the per-phase intent. Phases marked `*(shipped)*` have
> landed on `main`; phases without that marker may be either
> upcoming or shipped (this file is updated periodically, not
> continuously). For the **current authoritative shipped status**,
> see [`ReadMe.MD`](../../ReadMe.MD) — its "Shipped" section is
> kept in lock-step with `main` after each phase merges.

> **Post-Phase-5-Zero substrate note.** The Phase 0–3 task lists below reference SQLx — that was the substrate at the time those phases shipped. Phase 5-Zero retired SQLx in favor of `tokio-postgres` + `deadpool-postgres` + `postgres-types`. The framework today routes every connection-bearing call through `DjogiContext`, every row decode through `FromPgRow`, every test through `#[djogi::djogi_test]`, and every raw call through `ctx.raw_query` / `raw_scalar` / `raw_execute`. The historical task lists are kept verbatim because they document what shipped at each phase boundary; do not retro-edit them.

---

## Guiding Principles

1. **Each phase produces a usable, testable crate** — not a waterfall of unshippable code
2. **The model macro system (`djogi-macros`) is the foundation** — descriptor generation and model metadata land before higher-level APIs
3. **Raw SQL escape hatch ships in Phase 1** — the framework must never trap the developer
4. **Tests against real Postgres** — no mocking the database; use the `#[djogi_test]` harness (which spins up a per-test database, runs HeeRanjId schema install + node seed, and resets `heer.node_id` per test). The earlier-phase mention of `sqlx::test` is historical — `sqlx` was retired in Phase 5-Zero.
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
- [ ] Add `docker-compose.yml` with Postgres 18 for local dev and CI test databases
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
- [ ] Defer `#[model(pk = Serial)]`, `#[model(pk = RanjId)]`, `#[model(pk = None)]`, and composite keys until the default path is stable
  - [ ] For `0.1.0`, interpret this as: composite **primary** keys remain deferred, while composite **unique constraints/indexes** are expected to ship through the migration/index metadata path

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

- [x] Raw SQL escape hatch shipped — see `djogi::__bypass::RawAccessExt` (`raw_query`, `raw_scalar`, `raw_execute`, plus `raw_rows` / `raw_fetch_one` / `raw_ddl` / `raw_stream` / `raw_stream_with_fetch_size`) and `djogi::__bypass::RawPoolAccessExt` (`raw_pool` / `raw_conn` / `raw_with_client`). All require the `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute and `// JUSTIFICATION ...` comment at the call site; see [`docs/spec/raw-sql-escape-hatches.md`](raw-sql-escape-hatches.md) for the full contract. The pre-1.0 Phase 1d sketch listed three task names under a `djogi::raw` namespace that was never used — `RawAccessExt` is the shipped surface, with the names superseded as follows:
- [x] Typed-row raw read shipped as `RawAccessExt::raw_query<T>` (called `ctx.raw_query::<T>(sql, params).await`).
- [x] Scalar raw read shipped as `RawAccessExt::raw_scalar<T>`.
- [x] Side-effecting raw write shipped as `RawAccessExt::raw_execute`.
- [x] Raw methods route through `&mut DjogiContext` (which pattern-matches on pool-vs-transaction), so a single call site works for either backing — see `RawAccessExt` impls in `djogi/src/__bypass.rs`.

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

**Roadmap guidance on key shape:** For `0.1.0`, the intended through-model pattern is a surrogate single-column PK plus a composite unique constraint on the two relation columns. That delivers the production-critical properties for most M2M tables — pair uniqueness and efficient indexed lookups — without forcing composite primary-key complexity through CRUD, relations, admin, migrations, and live migration orchestration. A composite primary key can make sense later for a truly pure junction table that will never be referenced independently, but it is not the default roadmap direction.

**Deliverable:** FK with all cascade options, prefetch + select_related, M2M through models.

---

## Phase 4: Transactions & Expressions

**Goal:** Application-level transactions and field expressions for complex queries.

Phase 4 is the main query/write power inflection point. It is where Djogi stops being "typed CRUD plus a builder" and starts owning the concurrency and expression substrate needed for serious production workloads.

### 4a: Transaction API

- [x] `djogi::transaction::atomic(scope, |ctx| Box::pin(async move { ... }))` — closure-based, returns `Result<R, DjogiError>`. The closure receives `&mut DjogiContext` and returns `AtomicFuture<'_, R>` (= `Pin<Box<dyn Future<…>>>`). Implemented at `djogi/src/transaction.rs::atomic`; `IntoAtomicScope` is impl'd for both `&DjogiPool` and `&mut DjogiContext`. The pre-1.0 4a sketch listed a bare `async { ... }` shape; the real signature requires the boxed future.
- [x] Manual transaction driving — the framework path is `DjogiContext::from_pool(pool)` plus `atomic(...)`. Adopters who truly need to manually drive a transaction without `atomic` reach for `__bypass` per the raw-SQL escape-hatch contract.
- [x] Savepoint support within transactions — nested `atomic(...)` calls push savepoints (`djogi/src/transaction.rs`).
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

## Phase 7-Zero-2: Visage Query Surface + PK Refinements *(shipped)*

**Goal:** Freeze the PK substrate and make visages first-class query
entities before the Phase 7 migration engine and later consumer phases
build on those contracts.

- [x] Default PK flip: `#[model]` with no `pk` attribute now yields `HeerIdRecencyBiased`
- [x] Djogi-side naming surface: `HeerIdDesc` / `RanjIdDesc` re-exported publicly as `HeerIdRecencyBiased` / `RanjIdRecencyBiased`
- [x] `PrimaryKeyKind` + `pk_kind` descriptor contract
- [x] `PrimaryKey`, `PrimaryKeyDbGen`, `PrimaryKeyClientGen` trait split for built-in and custom PK kinds
- [x] `djogi::primary_key!` helper macro for custom PK definitions
- [x] Ambient PK usability: all built-in and custom PK kinds usable outside the PK slot
- [x] `bulk_create` pre-allocation retrofit via PK-kind dispatch and `generate_many`

### 7-Zero-2a: Visage Query Surface + Boundary Enforcement *(shipped)*

Phase 4.5 shipped visages as output-shape types only: `{Model}Public` / `SelfView` / `Admin` / `Export` carry `impl TryFrom<&Model>` for conversion but have no query-side surface. Phase 7-Zero-2 makes visages first-class query entities with compile-time scope enforcement, filling Phase 4.5's explicit M2M visage deferral in the process.

- [x] Visage as query entry point: `PublicRegisteredOwner::filter(|o| o.display_name.eq("Ada")).fetch_all(&mut ctx).await?` — every visage type gets the full QuerySet surface (filter, order, limit, fetch terminals) that models already have
- [x] Per-visage generated field-accessor types: alongside `{Model}Fields`, each visage emits `{Visage}Fields` surfacing only the fields the visage exposes. Attempting to reference a non-exposed field in a closure is a compile error, not a runtime omission
- [x] Forward-FK boundary enforcement: relation fields in the visage's accessor type point to visage-scoped peer accessors. `PublicRegisteredOwner::filter(|o| o.address.city.eq("Toronto"))` compiles; `.address.street` does not compile when `AddressPublic` exposes only `.city`. Enforcement is compile-time via the type system — zero runtime cost
- [x] Optional forward-FK / O2O relation-form visages: `Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` participate in the same boundary enforcement instead of remaining deferred
- [x] Reverse-FK boundary enforcement: symmetrical. From `AddressPublic`, the reverse accessor to owners returns `QuerySet<PublicRegisteredOwner>` where the closure receiver is `PublicRegisteredOwnerFields`
- [x] M2M boundary enforcement: visage-scoped M2M accessor methods return visage-scoped QuerySets. Through-model fields can opt into their own visage via `#[field(expose(...))]` on through-model fields, producing e.g. `UserInterestPublic`. Through-model visages participate in the same boundary enforcement. Fills the Phase 4.5 deferral "M2M visages — visages nest only through `ForeignKey<T>` / `OneToOneField<T>`; M2M stitching is manual"
- [x] SELECT narrowing: visage-scoped QuerySets emit SELECT with only the visage's exposed columns, not the full model. This is the headline performance win — visages stop being only an output shape and start paying off at the query side
- [x] Mutation scope: visage-scoped queries are read-only by default. `save` / `delete` / `update_or_create` on a visage emit a compile error pointing at the source model. Kept simple in v1; revisit only if a clear use case arrives
- [ ] Prefetch composition: an API for declaring a prefetch into a specific peer visage so chained traversals inherit the same boundary. Exact shape deferred to v2/v3 spec; leading candidates include a dedicated `prefetch_as::<PeerProjection>(model::relation)` terminal on the QuerySet and a generated `model::relation::as_public()`-style relation-path variant surfaced under each visage scope *(deferred — visage querysets are read-only in v1; cross-visage prefetch composition revisits in a later phase)*
- [ ] Interaction with §9b: Phase 9b's Tier 1 over-fetching detector gains a concrete suggestable fix — "you hydrated `RegisteredOwner` but only read `.display_name` + `.email` — swap for `PublicRegisteredOwner`" *(belongs to Phase 9b; left unchecked here as forward-reference)*

**Deliverable:** The pre-Phase-7 PK substrate is frozen, and visages become first-class query entities with compile-time FK / reverse-FK / M2M boundary enforcement and SELECT narrowing.

---

## Phase 5.5: Auth Substrate *(shipped)*

**Goal:** Framework-owned auth primitives that pair with Phase 5's tenant-key RLS and Phase 4's transaction substrate.

Shipped 2026-04-22 as squash `dfdfc7d` (PR #10). Ships `DjogiAuth` trait + `AuthContext` + `AuthError` + `PasswordHash` (feature `auth-argon2`) + automatic `set_tenant` with `applied_tenant_id` tracking + nested-atomic snapshot/restore + `_insecurely()` escape-hatch warnings. Session stores, token providers, and per-framework HTTP extractors (Axum, etc.) are deferred to a future adapter phase.

**Deliverable:** Every query inside an authenticated context automatically scopes to the correct tenant; password hashing is strongly-typed; `_insecurely()` only compiles where explicitly allowed.

---

## Phase 6: Spatial *(shipped)*

**Goal:** First-class typed spatial surface — `GeoPoint` runtime, PostGIS-backed radius/ordering predicates, automatic GiST index metadata, migration-emission contract for Phase 7.

Shipped 2026-04-22 as squash `b9e9860` (PR #11). Ships the `spatial` feature flag within the `djogi` crate — never a separate `djogi-spatial` crate per the locked one-crate rule. Surface: `GeoPoint { lat, lon }` with constructor validation + Haversine distance + WKT `Display`; manual 25-byte EWKB codec for `GEOGRAPHY(Point, 4326)`; `within_km(center, km)` / `order_by_distance(center)` with deterministic PK tiebreak; `OrderExpr` promoted to `#[non_exhaustive]` enum; `IndexSpec` extended with `requires_out_of_transaction` + `extension_dependency` migration-policy fields; `MigrationShape` contract helper that Phase 7's differ consumes. Zero new runtime dependencies beyond what Phase 6 integrated explicitly.

**Deliverable:** Model authors store `GeoPoint` fields, write radius filters against them, and order by distance — all type-safe, all feature-gated, all ready for Phase 7 migration emission to consume without further descriptor work.

---

## Phase 6.5: Spatial Polish

**Goal:** Two coupled deliverables — (1) the full default-feature grouped + windowed query surface Djogi still lacks (`.group_by` / `.having` / grouped `.order_by` / ROLLUP / CUBE / GROUPING SETS / `.over(window_spec)` / aggregate `.distinct()`); and (2) a `spatial`-gated PostGIS application layer (non-point geometries, shape predicates, bbox prefilter, distance-as-expression, named-region grouping, DBSCAN clustering, geohash bucketing, extension-aware `#[djogi_test]`). Together these make v0.1.0's spatial story a genuine differentiator rather than a point solution, and close the grouped/windowed query gap in the default ORM surface.

**Ordering.** 6.5 ships **before** Phase 7. The coupling is the descriptor contract (typed `GeographySubtype` discriminant on `FieldSqlType::Geography`, `IndexSpec` usage patterns) — contracts freeze upstream of their consumers, so 6.5 locks the final shape and Phase 7's migration differ is designed once against it. Phase 7's v3 plan must absorb 6.5's final descriptor shape; no "Phase 7 follow-up" task lives inside 6.5.

**SRID 4326 stays locked — matching Phase 6.** Arbitrary-SRID generalization is roadmap work (see `docs/roadmap/future-work.md` §4.6), explicitly kept out of 6.5 to avoid the ergonomic tax of const-generic SRIDs on the 95% of users who only need WGS84.

Default-feature surface (aggregation + windowing):

- [ ] `.group_by(|f| K)` → `GroupedQuerySet<T, K>`; `.annotate(|f| A)` → `GroupedAnnotatedQuerySet<T, K, A>`; terminals gated on the annotated state (compile-error on premature `.fetch_all`)
- [ ] `.rollup(|f| K)` / `.cube(|f| K)` / `.group_by_sets(|f| [K; N])` for multi-level aggregation in one Postgres pass
- [ ] `.having(|k, a| Expr<bool>)` / grouped `.order_by(|k, a| ...)` / `.limit` / `.offset` on `GroupedAnnotatedQuerySet`
- [ ] `AggregateExpr::over(|w| w.partition_by(...).order_by(...).rows(...).exclude(...))` — full ROWS / RANGE / GROUPS / EXCLUDE frame vocabulary
- [ ] `AggregateExpr::distinct()` on every aggregate variant that admits DISTINCT
- [ ] Annotation alias collision → clear runtime error naming both columns

Spatial-gated surface (`spatial` feature):

- [ ] Non-point geometries as first-class field types: `LineString`, `Polygon`, `MultiPoint`, `MultiPolygon` — each backed by `GEOGRAPHY(<Subtype>, 4326)` with an auto-emitted GiST index. `Polygon::closed(&[...])` auto-closes the ring; `Polygon::with_ring(outer)` / `Polygon::with_holes(outer, holes)` for power users
- [ ] `FieldSqlType::Geography` gains a typed `GeographySubtype { Point, LineString, Polygon, MultiPoint, MultiPolygon }` discriminant — migration differs compare subtypes by discriminant, not by `Display` text
- [ ] Shape predicates: `.contains` / `.intersects` / `.touches` / `.within(geometry)` — distinct from Phase 6's radius-based `within_km`; dispatch on `FieldRef<M, G: GeographyValue>` via a new sealed `GeographyValue` trait
- [ ] Bounding-box prefilter: `.bounded_by(min_lat, min_lon, max_lat, max_lon)` emitting the GiST-indexed `&&` overlap operator
- [ ] Distance-as-expression: `FieldRef<M, GeoPoint>::distance_to(&GeoPoint) -> Expr<f64>` composes with `.filter` / `.annotate` / `.order_by`
- [ ] Region grouping: `.group_by_region(|f| geo_field, R::objects())` / `.count_by_region(...)` — spatial-JOIN to a region model, returns `GroupedQuerySet<T, RegionKey<R>>` / `GroupedAnnotatedQuerySet<T, RegionKey<R>, i64>`
- [ ] Dynamic clustering: `.cluster_by_proximity(|f| geo_field, ClusterRadius)` → `GroupedQuerySet<T, ClusterId>` via `ST_ClusterDBSCAN`. `ClusterRadius::meters` / `ClusterRadius::degrees` / `.min_points`
- [ ] Geohash bucketing: `.bucket_by_cell(|f| geo_field, GeohashPrecision)` → `GroupedQuerySet<T, GeohashKey>` via `ST_GeoHash`. `GeohashPrecision::P1..=P12`
- [ ] `#[djogi_test(extensions = ["postgis"])]` attribute argument auto-provisions extensions at per-test DB creation time — removes the per-test `ctx.raw_ddl("CREATE EXTENSION ...")` pattern from Phase 6

**Deliverable:** Djogi ships a complete grouped / windowed aggregation surface in the default feature, and a PostGIS application layer (polygon containment, line adjacency, multi-geometry operations, bbox prefilter, distance-as-expression, region grouping, DBSCAN clustering, geohash bucketing) behind the `spatial` gate. Construction APIs bias toward "obvious correct default" with power-user escape hatches. Zero new runtime crate dependencies.

---

## Phase 7: Migration System *(shipped)*

**Goal:** Drift diagnostics, explicit SQL composition, target-scoped apply/rollback/repair.

Phase 7 shipped 2026-04-25 as task chain T1–T8 against the v3 plan:

- T1–T2 — schema snapshot data model + projection.
- T3 — diff + SQL emitter.
- T4 — runner core (advisory lock, transactional / non-transactional segments, ledger CRUD).
- T5 — rollback + fake + baseline + repair + verify.
- T6 — build.rs + compose + status (drift diagnostics + pending JSON).
- T7 — out-of-order policy + multi-DB guardrails + `attune` (`--record` / `--squash --from <ver>` with `--publish`).
- T8 — `db reset`, `db seed`, `djogi docs`, spec cleanup.

Filenames are `V<YYYYMMDDHHMMSS>__<slug>.sql` plus `.down.sql`. Every Phase 7 path keys per `(database, app)` bucket. SQL seeds (not Rhai) — the Rhai shell is a separate phase.

### 7a: Schema Differ

- [x] `ModelDescriptor` comparison: detect added/removed/altered fields, tables, indexes
- [x] `#[field(renamed_from = "old")]` for rename detection
- [x] `#[model(renamed_from = "old_table")]` for table rename detection
- [x] Destructive operation gating with `--allow-destructive`

### 7b: SQL Generation

- [x] Generate up/down SQL pairs from `SchemaDelta`
- [x] `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE ADD/DROP/ALTER COLUMN`
- [x] `CREATE INDEX` / `DROP INDEX` (with `CONCURRENTLY` support)
- [x] `CREATE TYPE ... AS ENUM` / `ALTER TYPE ... ADD VALUE`
- [x] `ADD CONSTRAINT` / `DROP CONSTRAINT` (with `NOT VALID` + `VALIDATE` support)
- [x] Foreign key constraints with cascade options

### 7c: Build-Time Integration

- [x] `build.rs` reads `target/djogi_models.json` and diffs against `schema_snapshot.json`
- [x] Emits compiler warning (not error) when drift detected
- [x] Never mutates migration files or snapshots directly
- [x] Stages pending composition artifacts under `target/djogi_pending/` for explicit CLI review

### 7d: CLI

- [x] `djogi migrations compose` — compose pending up/down SQL pairs from descriptor drift
- [ ] `djogi migrations apply` — deferred CLI dispatcher for applying pending migrations; library callers use `djogi::migrate::apply_plan`
- [ ] `djogi migrations rollback` — deferred CLI dispatcher for rolling back the last applied migration for one target
- [x] `djogi migrations status` — show file / snapshot / ledger / live-DB state for one target
- [ ] `djogi migrations verify` — deferred CLI dispatcher for snapshot, history, and live-DB verification
- [ ] `djogi migrations repair` — deferred CLI dispatcher for repairing failed or partially applied target-local migration state
- [ ] `djogi migrations baseline` — deferred CLI dispatcher for marking an existing schema as adopted without replay
- [x] `djogi migrations attune` — `--record` / `--squash --from <ver>` with `--publish`; localhost + dev-profile gates on squash
- [ ] `djogi migrations apply --fake` — deferred CLI dispatcher for marking a migration applied without running it
- [ ] `djogi db reset` — deferred convenience wrapper around drop + recreate + migration apply (triple-gated: localhost + non-production profile + explicit `--yes`)
- [x] `djogi db seed` — run `seeds/<database>/*.sql` files; `djogi_seed_runs` ledger; checksum-drift refusal; `--allow-non-localhost` opt-in
- [x] `djogi docs` — render Markdown reference pages from `inventory::iter::<ModelDescriptor>()` to `target/djogi-docs/<app>/<Model>.md`

Phase 7's migration CLI is explicitly target-scoped. App, CRUD-log, and event-log databases each have their own migration ledger, snapshot, and advisory-lock boundary. Cross-database foreign keys are rejected rather than modeled as first-class relations.

### 7e: Data Migrations

- [x] Support raw SQL data migrations (hand-written `.sql` files in `migrations/`)
- [ ] Support Rhai script data migrations (`.rhai` files using shell model API) — deferred to the Rhai shell phase

### 7f: Online / Zero-Downtime Migration Patterns

- [x] Phased migration execution model: the migration runner splits each generated migration into ordered step groups tagged transactional vs non-transactional. Transactional groups run inside `BEGIN/COMMIT`; non-transactional steps — `CREATE INDEX CONCURRENTLY`, `DROP INDEX CONCURRENTLY`, certain `CREATE EXTENSION` cases, some `ALTER TYPE ADD VALUE` operations — run outside any transaction. `atomic()` is available only around transactional steps; attempting to wrap a non-transactional step in `atomic()` produces a clear error ("this step cannot run inside a transaction — see Phase 7f phased-migration model") rather than a silent SQLSTATE from Postgres
- [x] Advisory-lock-based single-active-migration coordination (no two library apply sessions run concurrently against the same database target)
- [ ] Lock-timeout on DDL statements so blocked migrations back off rather than queue behind long transactions (`SET lock_timeout = '5s'` around each DDL) — folded into Phase 7.5's online-safety classification work
- [ ] Two-phase column rename: emit `ADD COLUMN new_name` + backfill from `old_name` + runtime reads both + drop `old_name` in a follow-up migration. Driven by `#[field(renamed_from = "old_name")]` + an opt-in `#[field(rename_strategy = "two_phase")]` — Phase 7.5 (`ExpandContract` classification)
- [ ] Two-phase type widening: add new-type column, backfill, cut over, drop old (analogous pattern)
- [ ] Safe NOT NULL addition: `ADD COLUMN ... DEFAULT value` (Postgres 11+ makes this fast-path without table rewrite) plus a `VALIDATE` pass for pre-existing-table columns
- [ ] Constraint addition with `NOT VALID` + `VALIDATE` as separate steps
- [ ] Backfill orchestration primitive: chunked `UPDATE ... WHERE pk BETWEEN $1 AND $2` with configurable chunk size, delay between chunks, progress reporting
- [ ] Destructive-op detection: dropping a column, dropping a table, narrowing a type — gated behind `--allow-destructive` (already in 7a) with an additional "migration is not online" warning emitted at composition time
- [ ] Backfill side-effect suppression: chunked `UPDATE` backfills run with outbox emission and audit writes suppressed by default. Migrations represent schema evolution, not domain events; firing outbox messages and audit rows for every historical row rewritten during a backfill is never the right default. Opt in per migration via an explicit `emit_side_effects = true` flag when the backfill genuinely is a business event

**Deliverable:** Full migration system with drift diagnostics, explicit SQL composition, target-scoped CLI, data migrations, and online migration patterns.

---

## Phase 7.5: Live-Migration Substrate + Protected Data Metadata & Field Codecs *(substrate shipped; operator runner deferred)*

**Goal:** Add descriptor-level protected-field semantics and storage transforms; add the live-migration substrate that classifies schema operations by online-safety and prepares safe rollout plans. Full operator-driven live execution (`djogi live run/resume/finalize`, status, abandon, daemon resume) is documented but deferred in v0.1.0.

Phase 7.5 expanded beyond the original protected-data scope to absorb the live-migration safety classifier (per the v3 plan). The shipped scope is the classifier/planning/protected-field substrate:

**Protected-data side (original scope):**

- [x] Add field metadata for sensitivity, redaction scope, rationale, and lifecycle class
- [x] Add descriptor support for field codecs such as encrypted/tokenized/custom-serialized columns
- [x] Ensure CRUD generation and row decoding apply codecs consistently
- [x] Integrate protected-field metadata with generated visages
  *(admin-defaults integration deferred to Phase 10 / Maahi)*
- [x] Emit compile-time diagnostics when sensitive annotations are underspecified

**Live-migration side (absorbed scope):**

- [x] `live_migrate::{plan, plan_file, state, classify}` substrate
- [x] `OnlineSafetyClassification` enum (`OnlineSafe` /
      `FastLockDestructiveGuarded` / `ExpandContract` /
      `OfflineOnly`, `#[non_exhaustive]`)
- [x] `pg_volatility` introspection module
- [x] Backfill engine and chunk-loop SQL pattern substrate
- [ ] `djogi live` plan resume and daemon-mode runner
      (documented/stubbed; operator runner deferred)
- [ ] `djogi live` CLI command bodies
      (show/status/run/resume/finalize/abandon documented/stubbed; not shipped)
- [x] `db cleanup-test-dbs`
- [x] EXCLUSION + stored-generated descriptor extension
      (`ExclusionConstraintSpec`, `GeneratedColumnSpec`,
      `ColumnSchema.generated`, `TableSchema.exclusion_constraints`,
      `SchemaOperation::{AddExclusionConstraint, DropExclusionConstraint}`,
      `ColumnChange::SetGenerated`)
- [x] T11 tactical bug-fix sprint
      (protected-data audit, tenant_key + ForeignKey RLS empty cast,
      `ForeignKey<T>` Serialize fix, reverse-accessor inherent-impl
      E0116 fix, `JsonbSchema` djogi:: alias acceptance)
- [x] T12 integration tests + per-phase compile-fixture dispatcher
      (originally a trybuild driver; migrated to lihaaf in Phase 8.5)

**Deferred:**

- T13 (catalog drift detection / runtime-vs-spec divergence audit) — pushed to Phase 8 or a dedicated bug-fix sprint

**Deliverable:** Djogi can classify schema operations by online-safety, emit the core live-migration plan/backfill substrate, and express protected-field intent once across generated surfaces. Operator orchestration via `djogi live` remains a deferred CLI surface.

---

## Phase 8: Hooks, Composition, Proxy, Computed Properties, Q-Algebra & Punnu

**Goal:** Lifecycle hooks, abstract model composition, proxy models, computed
queryable properties (both SQL-projectable and Rust-trait halves), public `Q<T>`
predicate algebra with XOR, programmatic `.exclude_struct()` parity, and `Punnu<T>`
typed-pool integration via the new `sassi` sibling crate.

**v0.1.0 publish gate.** Two-stage consumption:

- **During Phase 8 development through 8η**, djogi consumes sassi via a path dependency (`sassi = { path = "../sassi" }`) so the two repos can co-evolve.
- **At Phase 8.5 publish housekeeping**, sassi v0.1.0 publishes to crates.io first; djogi then flips its Cargo.toml to `sassi = "0.1"`, re-runs its full test sweep against the published crate to confirm no path-vs-published divergence, then publishes djogi v0.1.0.

v0.1.0 ships when **both** are true: (1) djogi Phase 8 complete through 8η plus Phase 8.5 publish housekeeping merged with green CI, including the post-flip re-test; (2) sassi v0.1.0 live on crates.io. The publish order within Phase 8.5 is sassi → djogi; that ordering is what the gate enforces.

### 8a: Trait-Based Model Hooks

- [ ] `impl ModelHooks for T` with `before_create`, `after_create`, `before_save`, `after_save`, `before_delete`, `after_delete`
- [ ] Hooks receive `&mut self` (before) or `&self` (after) + connection reference
- [ ] Optional — models without `impl ModelHooks` have zero hook overhead

### 8b: Abstract Model Composition

- [ ] `#[model(auditable)]` — adopter declares `created_by: Option<String>`; macro emits the `Auditable` trait impl + `__djogi_auditable_populate` hook (T2.4 superseded the original `#[derive(Auditable)]` surface — proc macros cannot observe sibling derives, so a single attribute opt-in is the canonical path)
- [ ] `#[model(soft_deletable)]` — adopter declares `deleted_at: Option<OffsetDateTime>`; macro emits the `SoftDeletable` trait impl. Adopter calls `.objects().not_deleted()` to filter; automatic default-filter composition deferred to 8γ T6 once `Q<T>` substrate lands (T2.6 superseded the original `#[derive(SoftDeletable)]` surface for symmetry with the auditable opt-in)
- [ ] Custom field group macros: developers can define their own derive macros that inject fields
- [ ] Constraint/index name interpolation: `%(model)s_%(field)s_unique`

### 8c: Proxy Models

- [ ] `#[model(proxy_for = "Vehicle")]` — shares parent table, different Rust type
- [ ] Custom default ordering on proxy
- [ ] Custom default filter on proxy (e.g., `WHERE active = true`)
- [ ] Different `ModelHooks` on proxy vs parent

### 8d: Computed Queryable Properties — both halves

#### SQL-projectable half (column-derivable computed properties)

- [ ] `#[computed(sql = "base_price * (1.0 + tax_rate)")]` — Rust getter + SQL expression
- [ ] Usable in `.filter()`, `.order_by()`, `.annotate()` — the macro wires both sides
- [ ] Not stored in DB unless `#[computed(sql = "...", stored)]` (Postgres GENERATED ALWAYS AS)

#### Rust-trait half (non-SQL-projectable derivations — bridges to 8f)

- [ ] `#[djogi::trait_impl]` macro on a trait impl block — registers the impl in the cross-type trait registry
- [ ] `Sassi::all_impl::<Trait>()` — query across every Punnu whose type registered an impl of `Trait`; returns `Vec<Arc<dyn Trait>>`
- [ ] `Punnu::scope().filter_impl::<Trait>()` — single-type trait queries restricted to entries impl-ing `Trait`
- [ ] Documentation: when to choose 8d-SQL vs 8d-Trait
  - SQL-projectable: depends only on existing columns; runs equally on backend (DB-side) and frontend (struct value)
  - Rust-trait: depends on Rust logic that can't be expressed in SQL; runs only on materialized objects via `.cache(&punnu)`

### 8e: Public Q-Algebra (NEW)

- [ ] `Q<T>` enum exposed in `djogi::query` — wraps `Q::Portable(PortablePredicate<T>)` plus djogi-only SQL extensions (Ilike, FullText, JsonbPath, Spatial, Regex, Expression, Array operators)
- [ ] Operator overloads: `&` (And), `|` (Or), `^` (Xor — NEW), `!` (Not)
- [ ] No public `Into<Q<T>> for sassi::BasicPredicate<T>` — predicates enter Djogi SQL/cache APIs only through Djogi-provenanced field closures or `PortablePredicate<T>`; the trusted portability walk lowers to `sassi::BasicPredicate<T>` only for Punnu replay.
- [ ] XOR SQL emit: boolean fast-path `a <> b`; general form `(NOT a AND b) OR (a AND NOT b)`
- [ ] `.exclude_struct(filter)` — programmatic counterpart to existing closure-based `.exclude(closure)`
- [ ] Internal `Condition` enum at `djogi/src/query/condition.rs` is **replaced** by `Q<T>` (behavior-preserving refactor — same SQL output for same queries)
- [ ] `LookupOp` split: Rust-evaluable ops (Eq/Neq/Gt/Gte/Lt/Lte/In/NotIn/IsNull/IsNotNull/Between/IContains/IStartsWith/IEndsWith/IExact) migrate into Djogi-owned `PortablePredicate<T>` and lower to Sassi only after provenance checks; SQL-only ops (Regex/IRegex via Postgres POSIX, FTS, JSONB path) stay in djogi `Q<T>`

### 8f: Punnu Integration (NEW — sassi sibling crate consumer)

- [ ] `.cache(&punnu)` modifier on `QuerySet` — opt-in cache gated by the same trusted portable-predicate reducer as `refresh_into`; returns `Err((self, PortablePredicateError))` for SQL-only querysets. On terminal methods (`.fetch_all()` etc.), results inserted into the bound Punnu
- [ ] `DjogiContext::punnu<T>()` — tenant-aware Punnu construction; same context returns the same Punnu instance per type
- [ ] Save/delete invalidation hooks via the existing `on_commit` substrate (anticipated per `docs/spec/orm-gap-analysis.md:620`); fire only on commit, not rollback
- [ ] Cross-tenant safety guards: panic in debug builds when accessing a Punnu created from a different tenant_key; `tracing::error!` + return empty in release
- [ ] `djogi::cache` re-export module — `pub use sassi::{Sassi, Punnu, MemQ, BasicPredicate, Cacheable, CacheBackend}` so users can `use djogi::cache::*` without explicitly adding sassi as a dep. Re-exported `BasicPredicate` is for direct Punnu use; Djogi SQL/cache entry points accept Djogi-provenanced `PortablePredicate<T>`, not arbitrary raw Sassi predicates.
- [ ] **Delta-sync via watermark field** — incremental cache refresh primitive. Builds on sassi §3.9.1 `MonotonicWatermark` + `DeltaSyncCacheable` + `DeltaPunnuFetcher` traits. Surface:
  - Watermark type is gated by sassi's `MonotonicWatermark` marker trait. Sassi crate ships std-only blanket impls (`SystemTime`, signed/unsigned integers `i32`/`i64`/`i128`/`u32`/`u64`/`u128`, tuples up to arity-4) plus opt-in feature gates for popular third-party time crates (`watermark-time`, `watermark-chrono`); `()`, `bool`, and `Duration` are deliberately not covered (the first two for trivial-Ord degeneracy, the third for elapsed-time semantics that don't match watermark contracts). djogi Models default to `Watermark = OffsetDateTime` (the type of `updated_at`); djogi's `Cargo.toml` declares `sassi = { version = "0.1", features = ["watermark-time"] }` so the `time::OffsetDateTime` blanket impl is present transitively for every adopter.
  - `#[derive(Model)]` auto-emits the `Cacheable` and `DeltaSyncCacheable` impls in one expansion (via the shared `sassi-codegen` library) — adopter writes no extra derive attribute, gets `fn watermark(&self) -> OffsetDateTime { self.updated_at }` for free. Adopters who want a different condition override via `#[model(... watermark_field = "<field_name>")]` (alternate field, type read from the named field — must satisfy `MonotonicWatermark` or compile error), or implement `DeltaSyncCacheable` manually for composite watermarks (e.g., `type Watermark = (OffsetDateTime, i64);` for transaction-tied disambiguation) and computed watermarks (e.g., `fn watermark(&self) { max(self.updated_at, self.deleted_at, self.reviewed_at) }`). Adopter custom types (Lamport clocks, BigInt newtypes) implement `MonotonicWatermark` themselves with a one-line marker impl.
  - `QuerySet<T>::refresh_into(self, punnu: &Punnu<T>, pool: deadpool::Pool<PgConnection>, auth: AuthContext) -> Result<DeltaRefreshHandle<T>, (Self, PortablePredicateError)>` — first gates the QuerySet through Djogi's trusted portable-predicate reducer. SQL-only `Q<T>` arms return the original QuerySet plus a typed portability error before a subscription starts. On success, the fetcher captures owned substrate (pool clone, AuthContext-by-value, and the QuerySet's Djogi-provenanced `PortablePredicate<T>` filter) and starts a `RefreshSubscription` on the Punnu. The fetcher does NOT capture `&mut DjogiContext` (that would require a non-'static lifetime). On each `update()` tick, the fetcher acquires a connection from the pool, constructs a fresh `DjogiContext` from `(conn, auth.clone())`, and dispatches by mode: initial no-watermark loads apply `WHERE <captured_filter>` and observe the unfiltered source high watermark, while watermark ticks fetch changed rows by `<watermark_field> >= since` plus any eviction-recovery id clause without re-applying the filter. Changed live rows are upserted into the Punnu even when they transitioned out of the captured predicate; subsequent cache reads apply `filter_basic` using the Sassi predicate lowered from the trusted `PortablePredicate<T>` to hide non-matches. Watermark field is read from the Model's `DeltaSyncCacheable::watermark` extractor — defaults to `updated_at` for djogi Models, configurable per the override paths above. The returned `DeltaRefreshHandle<T>` exposes `update()`, `update_full()`, `with_eviction_recovery(bool)`, `with_periodic_full_refresh(Option<NonZeroUsize>)`, and `cancel()` per sassi §3.9.1.
  - `Punnu<T>::start_delta_refresh(interval, fetcher)` — sassi-side, ships in sassi v0.1.0.
  - LRU-eviction handling: three independent composable knobs per sassi §3.9.1. (1) Sized-to-fit + warn-on-eviction is always on — one-shot `tracing::warn!` per `(Punnu, RefreshSubscription)` on first eviction collision, no overhead beyond an `AtomicBool` flip. (2) `RefreshSubscription::with_eviction_recovery(bool)` (default false) wires per-subscription event-subscriber + recovery query for high-churn workloads. (3) `RefreshSubscription::with_periodic_full_refresh(Option<NonZeroUsize>)` (default None) re-baselines watermark + refreshes LRU/schema drift via full re-fetch every Nth tick without (2)'s broadcast cost; hard deletes still require tombstones from soft-delete (Tracked) or outbox subscription paths. Adopters compose (2) and (3) based on workload shape; Phase 8 exposes both via the djogi-side `QuerySet::refresh_into` builder.
  - Compile-time enforcement: closure-flavored filters (anything that contains `MemQ::Closure`) and SQL-only `Q<T>` arms cannot construct a fetcher — `refresh_into` is gated on `PortablePredicate<T>`-only filters at the call site. Diagnostic points at the offending closure or non-portable predicate arm.
  - Deletion handling via tombstones (per sassi §3.9.1 "Deletion handling — tombstones, not absence"). The fetcher's `DeltaResult { items, tombstones }` carries explicit deletion notifications; sassi commits items + tombstones atomically via `Punnu::apply_delta`, where the tombstone-precedence rule evicts soft-deleted rows at commit time, emitting `PunnuEvent::Invalidate { id, reason: EventReason::OnDelete }` per evicted id. Sassi never infers deletion from absence (load-bearing for multi-subscription Punnu coherence — full-refresh is "delta with `since=None`" semantically and does NOT remove rows by absence). Three patterns djogi adopters compose: (1) soft-delete via `Tracked` — fetcher includes deleted rows in `items` so the delta-sync layer derives tombstones via `collect_tombstones`; sassi's `apply_delta` tombstone-precedence rule evicts soft-deleted rows at commit time. Application UI applies a visibility predicate (`deleted_at.is_null()`) over the cached scope via `MemQ::filter` — defensive against partial ticks, but the canonical post-commit cache state already excludes soft-deleted rows. (2) Outbox subscription — backend fetcher subscribes to djogi's outbox `OnDelete` stream, accumulates IDs into a local set drained as `tombstones` on the next `fetch_delta` call. Sassi applies via `Punnu::apply_delta`. Catches hard-deletes (DELETE FROM, no soft-delete trail) backend-side. (3) Periodic full-refresh — re-baselines watermark + refreshes LRU/schema drift; combine with (1) and/or (2) for deletion coverage (hard-deletes via outbox, soft-deletes via Tracked-derived tombstones).
  - AuthContext / RLS coherence: the fetcher's owned `AuthContext` is cloned per `update()` tick into a freshly-constructed `DjogiContext`, so each tick runs under the auth scope captured at subscription-construction time. Adopters rebuild the subscription on auth-scope changes (login/logout, tenant switch, role change) — the old `RefreshSubscription` is cancelled and a new one is constructed with the new `AuthContext`. djogi cannot enforce auth-rebuild on scope changes — surface in adopter-facing docs.

### Phase 8.5: Pre-publish housekeeping (v0.1.0 gate)

Lands after 8η predicate/cache correctness, before the publish run. Each item is its own small PR — no batching, so a single late breakage doesn't block the others.

- [ ] **Sassi v0.1.0 publish + djogi crates.io flip** — workflow:
   1. Sassi repo independently complete (test sweep green); tag `v0.1.0`; `cargo publish` from sassi; verify resolves on a clean machine.
   2. Djogi's `Cargo.toml` flips `sassi = { path = "../sassi" }` → `sassi = "0.1"` (no path or git dep).
   3. Djogi re-runs the full test sweep against the published crate to confirm no path-vs-published divergence.
   4. Only after step 3 passes does djogi cut its own `v0.1.0` tag.
- [ ] **Reference-symlink cleanup** — remove the design-mining `*-reference` symlinks at the djogi project root (django, sqlalchemy, diesel, sea-orm, sea-query, prisma, cot, alembic, flyway, liquibase, refinery). Keep only `HeeRanjID-reference` and `sassi-reference` (the actual integration siblings). `.gitignore` entries for `*-reference` stay as a re-introduction guard. Rationale: djogi goes public at v0.1.0; gitignored design-mining symlinks make the repo root look stale on github.
- [ ] **Docs sweep** — README accuracy pass against shipped surface, doc-link audit (no broken `docs/spec/` references), `cargo doc --no-deps` clean.
- [ ] **Drift-detection guide + apply-time pre-flight gate** — close djogi#152 before publish. Guide first: add `docs/spec/drift-detection.md` explaining how adopters should invoke `djogi migrations verify` across local dev, CI, deploy, and production-monitoring pipelines. Code second: `apply_plan` runs the existing D6xx `verify` pass before executing migration SQL and fails with a typed `RunnerError::DriftDetected { report }` on drift, with CLI output that points operators at the drift guide. This is catalog drift detection, not Phase 13's D7xx runtime-query prepare-protocol verifier.
- [ ] **Repo-flip readiness** — branch protection re-enabled (currently off per `project_djogi_gha_billing.md`), CI-required-for-merge configured, `act` workflow validation green.

**Deliverable:** Lifecycle hooks, composable field groups, proxy models, computed
properties (both halves), public `Q<T>` algebra with XOR + programmatic `.exclude_struct()`,
typed in-memory pool with cross-runtime predicate semantics via the sassi sibling crate.
**Phase 8 close → v0.1.0 publish (gated on sassi v0.1.0 also being live on crates.io).**

> **Phase 8 cluster status note (2026-05-08).** Phase 8α–8ζ have shipped on
> `main`. The pre-publish housekeeping originally sketched in 8g has been
> refactored into a dedicated **Phase 8.5** alpha-readiness cluster, while the
> remaining predicate/cache correctness work is now scoped to a separate
> **Phase 8eta (8η)** cluster — a correctness cluster, not release
> housekeeping. Phase 8eta reconciles the Sassi/Djogi predicate model (see
> *Predicate Portability* below), targets the predicate hard-gate issues
> (djogi #121, #126, #127, #107,
> #108, #109; sassi #15), and reduces the issue surface before 8.5. Phase 8.5
> then enforces a zero-open-issue gate across `djogi`, `sassi`, and
> `HeeRanjID` before the v0.1.0 publish runs. As of 2026-05-08 crates.io
> publishes the Sassi `0.1.0-beta.1` line; Sassi `main` has prepared a
> `0.1.0-beta.2` revision (metadata, postcard wire, Punnu entries export) but
> that revision has **not** been published. The 8eta predicate substrate is
> intentionally allowed to land in Sassi `main` before the next Sassi beta
> publish, so beta.2 (or its successor) can ship as a single bundled release;
> the eventual `sassi = "0.1"` crates.io flip in `djogi`'s `Cargo.toml`
> happens at the start of 8.5 publish housekeeping, not before.

### Predicate Portability

Portable predicates are built from Djogi root fields and can be evaluated
both by Postgres SQL emitters and by Sassi/Punnu in memory. PostgreSQL-
specific predicates on root fields are reached through
`explicit_pg_predicate()` so ordinary portable predicates still read like
ordinary database filters. Non-portable predicates include relation
traversal, regex, JSONB path, FTS, spatial/PostGIS, expressions,
aggregates, and raw condition bridges. Non-portable predicates remain valid
database queries, but cache and refresh boundaries reject them with typed
errors instead of broadening into full refreshes.

Example:

```rust
// Normal database query and portable cache predicate.
Post::objects().filter(|p| p.title().icontains("rust"));

// Valid database query, but PostgreSQL-specific and rejected by cache
// boundaries.
Post::objects().filter(|p| {
    p.title()
        .explicit_pg_predicate()
        .contains("é")
});

// Current spatial/PostGIS predicates are PostgreSQL-specific in 8eta.
Place::objects().filter(|p| {
    p.location()
        .explicit_pg_predicate()
        .within_km(center, 0.5)
});
```

Filtered refresh prevents stale matches. Initial loads fetch matching rows.
Delta ticks fetch rows changed by watermark or recovery id without applying
the portable filter, then upsert changed rows. A changed row that no longer
matches remains a canonical cached value but is excluded by later Punnu
`filter_basic` calls. Only true source deletes become tombstones.

The adopter-facing root field family is uniform: `{Model}Fields` accessors
return `DjogiField<Model, V>`, where a `DjogiField` carries a Sassi
`Field<Model, V>` for portable predicates and a Djogi `FieldRef<Model, V>`
for SQL-only operations. Relation and visage traversal go through separate
SQL-only field views (`{Model}SqlFields` / `{Visage}Fields`) because cached
root objects do not contain joined relation values; the route for cache-side
filtering by a related value is to project that value into a visage as a
real field.

---

## Phase 9: Shell, Analyzer & djqry

**Goal:** Interactive Rhai REPL, static query analyzer, and the djqry SQL override registry.

These surfaces are intentionally downstream of the core ORM/runtime. They are useful operational tools, but they must remain feature-gated adapters over the model/query layer rather than redefining the core identity of the crate. (The admin console is its own phase — see Phase 10 / Maahi.)

**Phase 9 amendment (2026-05-10).** Four architectural decisions land alongside the original Phase 9 task list:

1. **Shell is the primary query-construction surface for users**, not an admin REPL or occasional inspection tool. Adopters writing non-trivial query code spend more time in the shell than in their editor for the duration of that work. Shell startup latency is a product feature with measurable budgets (see `docs/spec/shell.md` §13.0); shell ergonomics (history, syntax highlighting, autocomplete on registered model methods, transparent SQL inspection, per-call timing) are first-class deliverables, not deferrable polish. The Rhai shell is also the workshop surface that other harnesses (lihaaf v0.1) defer to.
2. **djogi-as-dylib is a load-bearing Phase 9 dependency**, shared with lihaaf. The shell binary is small (~5 MB) and dlopens `libdjogi.so` (~30 MB) at startup. See `docs/spec/decisions.md` *Djogi-as-dylib for shell + lihaaf (Phase 9)* row and `docs/spec/shell.md` §13.11 for the rationale (build iteration, plugin ecosystem, memory hygiene, distribution size — explicitly NOT runtime query speed).
3. **`rhai-dylib` is the planned plugin-loading mechanism, evaluation pending** a 30-minute audit (§13.12) that confirms the crate's symbol-visibility requirements, Rhai-version compatibility, and maintenance status before djogi commits its `[lib]` configuration to satisfy it.
4. **Parse-vs-eval split for syntax-error UX** (§13.10). The shell calls `Engine::compile(&input)` first to surface syntax errors instantly with caret positioning, only then calls `Engine::eval_ast(&ast)` so the user never waits on a database round-trip to discover a typo. Ships with `Engine::set_strict_variables(true)` and an `OnVarFn` resolver that validates model-binding identifiers against the inventory-collected descriptor set.

### 9-Zero: Inventory-on-Dylib Spike

The dylib coupling is gated on a research spike that validates (a) `cargo rustc --crate-type=dylib` produces a working `libdjogi.so` for djogi's workspace, (b) `inventory::submit!` registrations made inside djogi propagate across the dylib boundary to dlopen-ing consumers, (c) the resulting dylib loads cleanly at runtime via `libloading` without TLS / global-init / loader-compat failures, and (d) the canonical Rhai API surface relied on by §9a (`Engine::compile`, `Engine::eval_ast`, `ParseError`, `Engine::set_strict_variables`, `OnVarFn`) behaves as the spec assumes.

- [x] **Spike artifact:** [`docs/research/2026-05-10-inventory-on-dylib-spike.md`](../research/2026-05-10-inventory-on-dylib-spike.md). 2026-05-10 outcome: **`GO_NATIVE`** — `cargo rustc -p djogi --lib --release --crate-type=dylib` produces working `libdjogi.so`; cross-DSO inventory propagation confirmed (`LIHAAF_SPIKE_TOTAL=2`, `BOTH_SUBMISSIONS_VISIBLE`); `libloading::Library::new` dlopen path confirmed for Phase 9 shell. **No djogi `Cargo.toml` changes required.** The four contingencies in `docs/spec/shell.md` §13.13 (`GO_WITH_MANIFEST` / `GO_WITH_WORKAROUND` / `RUNTIME_INCOMPATIBLE` / `NO_GO`) are retained as defensive design for revalidation cadence (next revalidation: every Rust toolchain MSRV bump + every 6 months absent other triggers)
- [ ] **API smoke step:** the spike artifact MUST include a runtime smoke test for the Rhai surface §9a depends on. Test exercises: (1) `Engine::compile(<bad_syntax>)` returns `ParseError` with line + column position; (2) `Engine::eval_ast(<good_ast>)` runs a registered model-binding function end-to-end; (3) `Engine::set_strict_variables(true)` causes mistyped identifier references to surface as parse-time errors before any runtime dispatch; (4) `OnVarFn` resolver receives the expected `(name, index, &EvalContext)` callback signature. Validates the spec's API claims against the Rhai version djogi-shell pins. Failure here surfaces a Rhai-API drift before §9a implementation begins
- [x] **Coordination with lihaaf:** lihaaf consumes the same dylib for its own reasons; spike runs once and feeds both Phase 9 and lihaaf's v0.1 spec ([`docs/spec/lihaaf-v0.1.md`](./lihaaf-v0.1.md), TBD)
- [x] **Contingency outcome (selected 2026-05-10):** `GO_NATIVE`. Other outcomes named for revalidation completeness (per `docs/spec/shell.md` §13.13):
  - `GO_NATIVE` — best case; no `Cargo.toml` changes needed (**selected**)
  - `GO_WITH_MANIFEST` — djogi's `[lib]` adds `crate-type = ["lib", "dylib"]`
  - `GO_WITH_WORKAROUND` — djogi exposes `pub fn lihaaf_inventory_collect_<T>()` per-collection re-exports; shared naming convention with lihaaf; spike must evaluate `linkme` / `ctor` / manual init alternatives before locking in the workaround
  - `RUNTIME_INCOMPATIBLE` — build succeeds but dylib fails at runtime (TLS init, loader compat, global-init races); same scoping as `NO_GO` but different remediation
  - `NO_GO` — Phase 9 ships statically-linked; dylib-dependent items deferred until toolchain blocker resolves (parse-vs-eval split, djqry authoring loop, ergonomics work all still ship; only the dylib-dependent items defer)

**Phase ordering (sequencing constraints).** Phase 9 milestones depend on upstream work landing in a specific order:

- **Phase 8ε wrap (set_role + snapshot signing + djogi verify) must complete before §9a implementation begins.** Rationale: the shell uses `DjogiContext`'s post-8ε surface for transaction-scoped role propagation in REPL transactions. §9a code that reaches into a pre-8ε `DjogiContext` would need rewrite when 8ε lands
- **`lihaaf v0.1` spec must land with self-review and independent review before any precompiled-Rhai-plugin (§9a-Plugins) work begins.** Rationale: the cross-harness boundary (lihaaf is Rust-only; Phase 9 owns Rhai test fixtures) needs both specs in agreement before either harness commits to ownership
- **Inventory-on-dylib spike must complete before §9a implementation begins.** Already gated above; recorded here for completeness
- **`rhai-dylib` audit (per shell.md §13.12) must complete before §9a-Plugins implementation begins.** Audit `PASS` enables §9a-Plugins; `FAIL` defers §9a-Plugins indefinitely (source-form Rhai modules ship regardless via the standard `.import` path)
- **§9b (Static Query Analyzer) and §9c (djqry SQL override registry) have no dependency on the dylib coupling.** They can land in parallel with §9a once Phase 8ε wraps

### 9a: Shell (Rhai REPL)

- [ ] `djogi shell` — launches REPL with all models loaded
- [ ] **Shell binary dlopens `libdjogi.so` at startup** (gated on 9-Zero spike outcome — `NO_GO` falls back to static linking for v0)
- [ ] Synchronous API via `block_on()` — no `.await` in shell
- [ ] **Parse-vs-eval split:** every submitted line goes through `Engine::compile` first; parse errors print a one-liner with caret positioning and skip `eval_ast` entirely. Only on parse success does the shell dispatch the AST. Removes the wait-then-fail loop on typos that today would round-trip the database before failing
- [ ] **Strict-variables mode:** `Engine::set_strict_variables(true)` plus an `OnVarFn` resolver that validates model-binding identifiers against the inventory-collected descriptor set. Mistyped model names (`Vechile::objects()`) surface as parse-time errors with caret positioning rather than runtime errors several lines into a script. Function-arity and argument-type errors remain runtime errors — Rhai does not expose a compile-time type checker for dynamic dispatch
- [ ] **Startup latency budget:** target sub-second cold start on a representative laptop; measured per-release and tracked. Justifies the dylib coupling (re-linking djogi statically on every shell-crate iteration would blow the budget repeatedly during development)
- [ ] **Ergonomics first-class:** persistent history, syntax highlighting, autocomplete on registered model methods, transparent SQL inspection (last-query echo, `EXPLAIN`-on-demand), per-call timing
- [ ] `pp(value)`, `sql("...")`, `begin()`, `commit()`, `rollback()`, `savepoint()`
- [ ] Error handling: one-liner + full traceback to `.djogi_shell_errors/`
- [ ] `.export` / `.import` / `.bookmark` for session scripts
- [ ] `djogi shell --run script.rhai` for headless execution
- [ ] **djqry authoring loop** — the shell is the primary surface for iterating on `djqry` overrides (§9c). Workflow: *test → optimize → compile → deploy*. Shell commands:
  - `djqry.export(<last_query>, "<name>")` — writes `djqry/<name>.sql` with frontmatter pre-populated from the last executed macro-query: `@name` set, `@on` inferred from the query's target models, `@replaces` captured verbatim, `@signature` computed, `@returns` inferred from the QuerySet's declared return type, `@binds` inferred from the filter closures, and the macro-generated SQL placed in the body as the starting point the author can optimize against
  - `djqry.import("<name>")` — loads an existing `djqry/<name>.sql`, parses its frontmatter + SQL, binds the override into the shell session as a callable, and runs it alongside the macro-query form for side-by-side comparison (row count, first-row diff, timing)
  - `djqry.diff("<name>")` — runs macro-query and override both, reports result-set diff + `EXPLAIN` cost comparison + timing. Acts as the local on-demand analog of CI's `djogi djqry verify`
  - `djqry.sign("<name>")` — re-computes the fingerprint from the current `@replaces` and updates `@signature`, asserting the author has re-verified. Prompts for confirmation before overwriting
- [ ] **`rhai-dylib` audit** (gates §9a-Plugins below): 30-minute audit of `rhai-dylib` (https://crates.io/crates/rhai-dylib) covering symbol-visibility requirements, `pub extern "Rust"` annotations, Rhai-version compatibility, dylib-loader compatibility (likely `libloading`), and maintenance status. Audit outcome documented in the 9-Zero spike artifact alongside contingency selection. Audit failure scopes Phase 9 to source-form Rhai modules only; precompiled `.so` plugins defer until an alternative crate or upstream fix lands

### 9a-Plugins: Precompiled Rhai Module Loading (gated on `rhai-dylib` audit)

- [ ] Adopters package query-helper Rhai modules as precompiled `.so` artifacts that link against the canonical `libdjogi.so`
- [ ] Shell loads precompiled modules from a configured search path at startup (or on `.import`)
- [ ] Per-module memory cost stays roughly constant as the ecosystem grows (each plugin shares the loaded djogi instead of statically baking its own copy)
- [ ] Source-form `.rhai` modules continue to load via the existing `.import` path; the precompiled path is purely an optimization
- [ ] **`.rhai` test-fixture surface** — Phase 9 owns the in-process Rhai parse + snapshot-compare harness for testing shell scripts. Lihaaf is Rust-only and explicitly does not host this surface (~300-500 LOC, in-process, NOT subprocess-based; design lives with the shell because the shell already owns the Rhai engine, the model bindings, and the runtime). Detailed design pending — referenced here so the cross-harness boundary is unambiguous

### 9b: Static Query Analyzer

The analyzer ships as two tiers with different fidelity guarantees. Tier 1 is mainline and intended for CI gating by default. Tier 2 is experimental and best-effort — surfaced as warnings, never as `--deny` targets unless explicitly requested.

**Data sources.** Call-site discovery comes from source AST via `syn`. Model metadata (FK topology, visage maps, field descriptors) comes from `target/djogi_models.json`, which is emitted by the existing `#[model]` + `build.rs` pipeline during a normal `cargo build`. The analyzer requires a successful build to run — the metadata file is the FK graph's authoritative source, not a guess inferred from AST.

- [ ] `djogi analyze query` — walks every crate in the workspace, parses `.rs` files with `syn`, finds every QuerySet terminal (`.fetch_all`, `.fetch_one`, `.first`, `.exists`, `.count`, `.delete`, `.update`, `.stream`) and every `raw_query` / `execute_raw` call site

**Tier 1 — mainline, high-signal, low-false-positive (syn + metadata file, no type resolution needed):**

- [ ] Loop-shape N+1 detector: flag any terminal whose AST ancestor chain includes a `for` / `while` / iterator `.map` / `.for_each` closure. Receiver-type resolution is best-effort; when the receiver is unambiguous (e.g., `User::filter().fetch_all()`), the suggestion message names the FK and points at the `.prefetch()` call that would replace it. When the receiver is generic or goes through a helper, the lint still fires but with a softer message
- [ ] `.fetch()` vs `.prefetch()` misuse: when `.fetch()` appears inside an iterator over a parent collection whose FK is declared, point at `.prefetch()` + the exact `Related` accessor to use instead
- [ ] Over-fetching detector: when a QuerySet hydrates a full `Model` and the same scope only reads a small, enumerable subset of fields on the result, suggest the matching visage type (declared via `#[model(expose(...))]`) or propose a new `expose` group. Conservative: only fires when the post-hydration field access is fully visible in the AST; silent otherwise

**Tier 2 — experimental, opt-in, best-effort graph-aware analysis:**

- [ ] Graph-aware repeat-node detection: the descriptor registry's FK topology (from `target/djogi_models.json`) is a directed graph of tables-as-nodes and FKs-as-edges. Within a scope (function body, `async` block, `atomic()` closure), the analyzer attempts to track the set of `(model, filter_fingerprint)` pairs reached by terminals. Where receiver types resolve cleanly via `syn`, repeat visits to the same node — whether from independent call sites, through different FK traversals, or across prefetch chains that partially overlap — are flagged with a suggestion to hoist the fetch, fold the filters, or cover both accesses with a unified `select_related` / `prefetch_related` chain
- [ ] Honest caveat: `syn` alone cannot fully resolve receiver types through generic wrappers, re-exports, or helper indirection. When the analyzer cannot resolve a receiver, it silently skips rather than guessing. Coverage is documented as "high-signal when receiver is unambiguous; silent otherwise". A future upgrade path — rustc/HIR or `rust-analyzer`-as-a-library — is named in the follow-up list but not a Phase 9b deliverable

**Output + gating:**

- [ ] Output modes: `--format human` (colorized, grouped by file), `--format json` (machine-readable for editor integration), `--format clippy` (compatible with `cargo clippy --message-format json`)
- [ ] Severity gating: `--deny <lint>` turns a Tier 1 warning into a non-zero exit code for CI. Tier 2 lints default to warn-only; `--deny experimental` is an explicit opt-in for teams willing to accept Tier 2 false-positive risk
- [ ] Scope: pure static analysis beyond what a `cargo build` already produces. No database connection, no query execution. The pre-existing `target/djogi_models.json` build artifact is the only runtime input

### 9c: `djqry` SQL Override Registry

When a multi-hop macro-query compiles to a plan that is significantly worse than a hand-written query, the escape hatch today is `ctx.raw_query::<T>(...)` — which fragments the codebase visually and decouples the site from descriptor-aware tooling (static analyzer, admin surface, observability labels). `djqry` keeps the hand-tuned SQL in its own file while surfacing it as a typed method on the relevant models, preserving the declarative call-site shape elsewhere and giving the override the same type-safety, tracing, and analyzer treatment as macro-generated queries.

- [ ] `djqry/` directory at repo root holds `.sql` files; each file declares one override via frontmatter header comments
- [ ] Frontmatter schema: `@name` (method name, snake_case), `@on` (comma-separated list of models and / or visages; `_global` for non-model-scoped overrides), `@returns` (Rust type implementing `FromPgRow`), `@binds` (positional bind types — `()` for none), `@replaces` (multi-line canonical macro-query the override optimizes — documentation plus drift-check source), `@signature` (fingerprint hash bumped on manual re-verification)
- [ ] Build-time generation: a new stage in the existing `build.rs` pipeline (alongside `target/djogi_models.json` emission) parses every `.sql` file, validates frontmatter against descriptor metadata, and emits a generated `{Model}Djqry` zero-sized type per owner with one associated async function per override. Call site reads `VehicleDjqry::expired_registrations(&mut ctx).await?` — parallel to Phase 2's `{Model}Filter` and Phase 3's `{Model}Related` generated types, which is the established convention for per-model namespaced helpers. The `Djqry` suffix is distinctive, grep-able, and zero collision risk. For `@on: _global` overrides the parallel type is `GlobalDjqry`: `GlobalDjqry::fleet_stats(&mut ctx).await?`
- [ ] Multi-owner: when `@on:` lists several owners, delegating methods are generated on each. All delegates resolve to the same compiled SQL; the graph-aware Tier 2 of §9b uses the `@on:` list to reason about which node-visits the override covers
- [ ] Drift detection — mandatory: the build pipeline re-computes the AST-shape fingerprint of `@replaces` (structure plus types plus FK topology from `target/djogi_models.json`, not filter literals) and fails the build when it diverges from the stored `@signature`. Failure message names the model graph before and after, asks the author to re-verify, and suggests a new signature value to copy
- [ ] Drift detection — opt-in: `djogi djqry verify <name>` runs the macro-query and the override against a live database, diffs result sets, reports. CI gates on this; local builds skip it for speed. Local devs may run it on-demand when bumping a signature
- [ ] Runtime dispatch: each generated method routes through `ctx.raw_query::<T>(...)` (Phase 5 substrate) and decodes via `FromPgRow`. An override-firing tracing event names the override so Phase 11b / 11e observability surfaces highlight hand-tuned queries distinctly from macro-generated ones
- [ ] Error modes flagged at build time: missing required frontmatter field, unknown `@on` owner, `@returns` type missing `FromPgRow`, `@binds` arity mismatch with `$N` placeholder count in SQL, reserved-name collision with framework-generated methods, `@signature` mismatch
- [ ] Scope limits: v1 is read-only (SELECT-shaped overrides). `UPDATE` / `DELETE` / `INSERT` overrides deferred until a concrete use case surfaces — raw `ctx.execute_raw` remains available in the interim
- [ ] Authoring loop lives in the shell (§9a): `djqry.export`, `djqry.import`, `djqry.diff`, `djqry.sign` close the *test → optimize → compile → deploy* cycle inside the REPL. Authoring a new override never requires leaving the shell to hand-craft frontmatter — `export` captures the canonical macro-query, infers `@returns` / `@binds` from the QuerySet's declared types, computes the initial `@signature`, and seeds the SQL body with the macro-generated query as the baseline for optimization

**Deliverable:** Working shell, `djogi analyze query` lint pass, and `djqry` SQL override registry surfaced as typed model methods with a shell-native authoring loop. (Admin console — Maahi — is Phase 10.)

---

## Phase 9.5: Data Lifecycle & Governance

**Goal:** Turn lifecycle metadata into reviewable operator workflows.

- [ ] Add model/field lifecycle classes for purge, anonymize, archive, and permanent retention
- [ ] Generate dependency-aware lifecycle plans from model descriptors
- [ ] Add legal-hold primitives that override generated lifecycle plans
- [ ] Expose CLI planning/review/apply workflows for lifecycle operations
- [ ] Ensure lifecycle operations emit audit and event records

**Deliverable:** Djogi can plan and execute safe data-lifecycle operations without embedding product workflow logic in app code.

---

## Phase 10: Maahi — Admin Console

**Goal:** Auto-generated admin console (Maahi) with its own visage-grant RBAC layer (visages remain pure compile-time projections — Maahi is the runtime authorization system that consumes them), multi-tenancy-aware, with a first-class security floor (CSRF triple stack, session rotation, server-side write enforcement, visibility-aware `Label` trait, inline-bulk approval threshold).

The full design is in [`docs/spec/maahi/`](./maahi/index.md). Maahi ships as the `djogi-maahi` workspace crate behind the existing `admin` feature flag; per the carve-out reasoning in `CLAUDE.md`, Maahi is the lone admin-tier exception to the one-djogi-crate rule.

### 10a: Crate Substrate + Auth

- [ ] `djogi-maahi` workspace crate scaffolded; `djogi`'s `admin` feature pulls it in as optional dep; `djogi::maahi::*` re-exports
- [ ] `_admin_users` / `_admin_sessions` / `_admin_roles` / `_admin_role_visage_perms` / `_admin_role_model_perms` / `_admin_pending_actions` schemas in the audit DB (per `docs/spec/maahi/architecture.md`, `rbac.md`, `operations.md`); explicit `ON DELETE RESTRICT` on `_admin_users.role_id` and `_admin_roles.parent_role_id`; `ON DELETE CASCADE` on the two `_admin_role_*_perms` tables; `_admin_sessions.token_hash` is HMAC-SHA256 keyed by `session_secret_env` (UNIQUE INDEX); `_admin_pending_actions` ships with partial-unresolved + `expires_at` indexes
- [ ] `djogi admin set-password --superuser <email>` bootstrap CLI; `reset-password`, `build`, `info` companions

### 10b: Permission Model + Feasibility Analysis

- [ ] Six-action permission resolution (Create / Read / Update / Delete / BulkUpdate / BulkDelete) with per-`(role, app, model)` overrides via `_admin_role_model_perms` (keyed `(role_id, app_name, model_name)` to match the visage-grant qualification axis; v1 enforces workspace-wide model-name uniqueness per `apps-and-database-domains.md` "Cross-App FK Graph (T9)" so the lookup is unambiguous on `model_name` alone today, but the resolver always carries `app_name` to stay forward-compatible with the deferred descriptor-shape change that lets two apps share a short model name and resolve independently)
- [ ] Visage-grant resolution: `_admin_role_visage_perms` rows per `(role_id, app_name, model_name, visage_name, can_view, can_edit)`; effective visible / editable field sets computed as the union across granted visages, optionally extended by `view_full_struct` / `write_full_struct`, always minus `expose(none)`
- [ ] Single-parent role inheritance with cycle rejection on save and "this affects N child roles" save-time preview; role-deletion UX (reassign-first) for users and child roles
- [ ] Compile-time / startup feasibility analysis: five checks per `(role, app, model)` triple (`can_actually_read` / `_delete` keyed on the effective **visible** field set; `can_actually_update` / `_create` keyed on the effective **editable** field set per `rbac.md` Effective Permission Resolution — `_create` requires the editable set to cover every NOT NULL no-database-default field; plus `fk_label_reachable` for FK fields, with FK targets resolved through the apps registry to the owning `(target_app, target_model)`) surfaced as `AppDiagnostic` entries; UI affordances hidden when feasibility fails
- [ ] Visage-drift handling on deploy: missing compiled visages flagged as `AppDiagnostic`, dangling `_admin_role_visage_perms` rows treated as no-op until removed or the visage restored

### 10c: Field-Visibility Substrate + Label Trait

- [ ] `expose(none)` enforced as the absolute floor — never UI-rendered, never editable, even for superuser, even with `view_full_struct` / `write_full_struct`
- [ ] `Label` trait + `VisibleFields` parameter live in `djogi` (not `djogi-maahi`); `#[model]` macro emits the impl per the four-rule resolution chain (`label_fn` > `#[field(label)]` > `String`-fallback > ID-only); concurrent `label_fn` and `#[field(label)]` is a compile error
- [ ] FK widget tier resolution (preload / typeahead based on `[admin].fk_preload_threshold`); optional `AdminFkFilter` trait + `#[field(admin_fk_filter = "...")]` override; FK dropdowns render row labels via `Label::label(&visible)` with `visible` constructed from the requesting role's effective visibility on the FK target
- [ ] List view default column and audit-log entry rendering route through `Label::label(&visible)` constructed from the *viewer's* visibility on the source model

### 10d: Multi-Tenancy

- [ ] Auto-detection of multi-tenant mode from registered RLS-enabled models; `[admin].multi_tenant` config override
- [ ] `_admin_sessions.current_tenant_scope` records the per-session active tenant; middleware calls `set_tenant(session.current_tenant_scope)` on every server-fn dispatch
- [ ] Cross-tenant login flow: short-lived signed one-time login ticket bridges credential check → tenant pick (no session row written until pick); hidden in single-tenant deployments

### 10e: Dioxus Renderer

- [ ] Dioxus full-stack components: list view, ModelForm, M2M inline, JSONB nested editor, `AdminClean` validation hook
- [ ] Role-config UI: hierarchical app → model → visage view/edit checkbox grid, per-model action overrides, system-permission toggles, `Preview Effects` action that walks every model the role can see and shows the resolved field set + action bits
- [ ] `djogi admin build` WASM bundle pipeline (`dx bundle` integration)
- [ ] CSRF triple stack (SameSite=Strict + `X-Maahi-CSRF` custom header + Origin/Referer check)
- [ ] Session rotation on login / password change / role change / tenant switch
- [ ] Server-side write enforcement that rejects out-of-editable-set fields explicitly (not silent filter)
- [ ] Two parallel login rate limiters (per-IP and per-email, both must accept); `login_rate_limit_per_ip` and `login_rate_limit_per_email` config keys; multi-instance deployments require shared state

### 10f: Approval Flow

- [ ] `_admin_pending_actions` table with two v1 action kinds (`BulkDelete`, `InlineSave`) sharing queue + lifecycle + dual-control discipline
- [ ] Magnitude-confirmation prompt on `BulkDelete` and `BulkUpdate` ("type the count to confirm")
- [ ] Inline-bulk threshold (`[admin].inline_bulk_threshold`, default 25) routes mass-removal saves through the approval flow as `InlineSave`
- [ ] Approver coverage rule: approver must hold every action permission the package execution requires (anti-piggyback)
- [ ] Single-admin / bootstrap deployments cannot satisfy approver ≠ requester; bootstrap flow recommends provisioning a second admin with the full action set required for the approval-gated operations they must approve

### 10g: System Permissions + Audit Access

- [ ] Four v1 system permissions: `view_audit_log` (visibility-filtered read of `{snake_case(model)}_logs` tables per `logging.md` §9.1), `manage_users` (five-clause upper-bound rule covering `is_superuser`, `system_perms` subset, effective per-`(app, model, action)` subset, effective visage-grant subset, and tenant-reach — both subset axes app-qualified to match the apps subsystem), `view_full_struct` (read all non-`expose(none)` fields independent of visage view grants), `write_full_struct` (edit all non-`expose(none)`, non-`admin_readonly` fields independent of visage edit grants; requires `view_full_struct`)
- [ ] `{snake_case(model)}_logs` read access through Maahi UI for `view_audit_log` holders, with field-level visibility computed from the viewer's effective visage grants plus any `view_full_struct`, scoped to their tenant

**Deliverable:** Production-grade Maahi admin console with visage-grant-driven visibility, multi-tenancy with secure cross-tenant login handoff, descriptor-driven UI, dual-control approval gates on `BulkDelete` and `InlineSave` with approver-coverage discipline, and four v1 system permissions (`view_audit_log`, `manage_users`, `view_full_struct`, `write_full_struct`).

---

## Phase 10.5: Maahi Compliance & Delegation

**Goal:** Layer compliance and delegation polish on Phase 10 v1 without breaking changes. Brings Maahi to enterprise-compliance grade and Django-parity feature breadth.

Full deferral list at [`docs/spec/maahi/phase-map.md`](./maahi/phase-map.md).

### 10.5a: Advanced Delegation

- [ ] Multi-parent role inheritance with diamond resolution rules
- [ ] `manage_roles` system permission with transitive upper-bound delegation (every grant ≤ granter's privileges, recursively through inheritance)
- [ ] Frozen / locked roles for orgs sensitive to inheritance cascades

### 10.5b: Broader Approval Workflows

- [ ] Approval workflows beyond `BulkDelete` and `InlineSave` (configurable per action / per model)
- [ ] Approval-queue UX polish: per-role notifications, bulk approval, queue search and triage

### 10.5c: Audit Retention + Redaction

- [ ] Scope-aware audit retention (different retention per source-model scope)
- [ ] Scope-aware audit redaction (further restrict viewable fields in historical entries based on data-classification rules)

### 10.5d: Django-Parity Features

- [ ] `list_select_related` (FK eager-loading on list view; auto-detect from `admin_list_display`)
- [ ] `raw_id_fields` equivalent (third FK widget tier above typeahead — no-widget-just-ID with popup search)
- [ ] `fields` / `fieldsets` (explicit form-field ordering and grouped sections)
- [ ] `AdminAction` extension trait for custom bulk actions (paralleling `AdminClean`, `AdminFkFilter`)
- [ ] Per-row history view (audit-log drill-down for a single record)
- [ ] `list_editable` (inline-edit columns from list view)
- [ ] `prepopulated_fields` (auto-populate fields from other fields)
- [ ] `date_hierarchy` (date drill-down on list views)
- [ ] Inline polish: `extra`, `min_num`, `max_num`, per-relation `can_delete`
- [ ] `view_on_site` (link from admin row to public URL — extension-trait or core)

**Deliverable:** Maahi reaches Django-parity feature breadth and enterprise-compliance grade approval / delegation surface.

---

## Phase 11: CRUD Logging & Observability

**Goal:** Automated audit trail plus concrete observability hooks (tracing, metrics, slow-query callbacks) that apps can integrate with standard Rust observability crates.

### 11a: Audit Trail

- [ ] Three-database architecture: app, crud_logs, event_logs (pools already defined in Phase 0/1)
- [ ] Profile-first logging config: `light`, `balanced`, `strict_audit`; advanced per-sink overrides only as escape hatches
- [ ] Per-model `#[model(crud_log = true)]` — auto-provision mirror `_logs` table
- [ ] JSON-aware diffing with dot-notation paths through `Jsonb<T>` nesting
- [ ] Actor attribution via `save_with_actor()` or request-context hook
- [ ] Make CRUD delivery semantics explicit: best-effort, durable bounded retry, or fail-closed depending on profile
- [ ] Surface sink health and degraded mode clearly in metrics / CLI / tracing output
- [ ] Document and enforce that strict audit means rejecting app writes when required CRUD audit cannot be satisfied, not cross-database atomic commit

### 11b: Tracing Integration

- [ ] Emit a `tracing::Span` per query with fields: `sql_text` (truncated, no bind values), `duration_ms`, `rows_affected`, `pool_wait_ms`, `model_name` (when derivable)
- [ ] Span attachment to surrounding `atomic()` scope's span (so transactions appear as parent spans over their queries)
- [ ] Opt-out per model via `#[model(trace = false)]` for hot-path tables

### 11c: Slow-Query Callbacks

- [ ] `djogi::observe::register_slow_query_handler(threshold: Duration, handler: impl Fn(&QueryTelemetry))`
- [ ] `QueryTelemetry` carries: sql, duration, row count, backend pid, lock wait time, which connection pool
- [ ] Guaranteed called after query completion (success or error); handler runs on the query task's executor

### 11d: Metrics Emission

- [ ] `metrics` crate integration: histograms for query duration, counters for rows affected, gauges for pool utilization + idle vs active connections
- [ ] Per-model breakdown labels (opt-in via `#[model(metrics = true)]`)
- [ ] Pool-level metrics per the three-pool architecture

### 11e: Admin-UI Observability Views

- [ ] Phase 10's admin layer (Maahi) surfaces slow-query log, pool stats, long-running transactions, recent `crud_logs` entries for a given record — provided the observability hooks from 11b/11c/11d are wired
- [ ] Zero additional cost when the admin feature isn't enabled; the hooks stand alone
- [ ] Per-request debug drawer (gated on `dev_mode = true` + `admin` feature flag): bottom panel on every `/_admin/` page showing queries issued during the request, per-query duration, originating `tracing` span, rows returned, and a SQL-text preview with binds inlined for readability
- [ ] Click-to-EXPLAIN: each drawer row exposes an "Explain" action that runs `EXPLAIN (FORMAT JSON)` by default — pure planner inspection, no execution, zero side effects regardless of statement kind. An explicit "Explain with Analyze" opt-in is available for SELECTs only; for INSERT/UPDATE/DELETE the `ANALYZE` variant is disabled in the UI with a visible note that `EXPLAIN ANALYZE` executes the statement and that non-transactional effects (`nextval` advancement, `LISTEN/NOTIFY`, deferred trigger side-channels) are not reclaimed by a wrapping savepoint. Plans render as a collapsible tree with per-node cost and row-count estimates
- [ ] Semantic N+1 flag: because Djogi knows the FK topology at compile time, the drawer annotates any relation fetched more than K times within a single request span with the exact model + FK name and the `.prefetch()` call that would collapse it — no pattern-matching heuristics, the detection is driven by declared structure
- [ ] Dev-only scope: the drawer is feature-flagged out of release builds and has no staging/canary mode. Non-dev environments rely on §11b/11c/11d (tracing spans, slow-query callbacks, metrics) for query visibility. If a team wants drawer-like introspection in staging, that is a separate future item, not a Phase 11e deliverable
- [ ] Optional middleware hook (shipped under each web-framework sub-feature flag — `axum`, `warp`, etc.) that injects the drawer into any HTML response in dev mode, not just admin pages. API-only apps get per-request correlation via a stable request ID — the middleware generates an ID per request and the response carries it in a compact `X-Djogi-Queries` header of the form `id=<token>; count=12; slow=2; total_ms=47`. Full per-query detail is retrieved (dev-mode only) by calling `GET /_djogi/debug/request/<id>`, which looks up the trace in a bounded in-memory ring buffer keyed by ID. This is correlation-safe under HTTP/1.1 keep-alive, HTTP/2 multiplexing, client-side connection pooling, and multi-instance deployments where "most recent on this connection" would be ambiguous or racy. Ring buffer size is configurable with a sensible default (128 entries, oldest-evicted); entries carry the full query list, per-query durations, binds, and the originating tracing span ID

### 11f: Event Logging

- [ ] Event logging via `tracing` subscriber layer writing to the event log database
- [ ] Schema for events: timestamp, level, target, fields, parent span id
- [ ] Retention policy opt-in (delete events older than N days)
- [ ] Keep event logging best-effort in built-in profiles; expose dropped-event counters and sink-failure warnings

### 11g: Log-Database Operations

- [ ] Unified operator workflow for app / CRUD-log / event-log migrations with explicit per-database labeling
- [ ] `db reset` remains app-first; touching logging databases requires explicit flags
- [ ] Startup checks honor profile semantics: `light` tolerates missing sinks, `balanced` starts degraded with warnings, `strict_audit` refuses startup when required CRUD audit sink is unavailable

**Deliverable:** Audit trail + tracing spans + slow-query hooks + metrics + admin dashboards + event logging.

---

## Phase 11.5: Operational Tooling

**Goal:** Turnkey solution for the boring-but-critical operational work every Postgres app needs — backups, vacuums, maintenance schedules, disaster recovery drills. Without this, teams hand-roll it inconsistently and find out in production it was wrong.

### 11.5a: Scheduled Backups

- [ ] `djogi ops backup setup --daily [--weekly] [--retention 14d]` — generates a platform-appropriate scheduler config (cron fragment, systemd timer unit, or launchd plist) + a backup script that wraps `pg_dump --format=custom` with sane defaults (parallelism, compression)
- [ ] `djogi ops backup now` — one-shot manual backup
- [ ] `djogi ops backup verify <file>` — runs `pg_restore --list` to confirm the archive is restorable
- [ ] Storage targets: local path, S3-compatible (via env-var-configured endpoint + credentials), optional `rclone` passthrough
- [ ] Retention policy enforcement (prune backups older than configured retention)

### 11.5b: Point-In-Time Recovery (opt-in)

- [ ] `djogi ops pitr setup` — configures WAL archiving to a specified target, generates `restore.conf` template
- [ ] `djogi ops pitr restore --target-time '...'` — restore drill runbook that produces a new database at a specific wall-clock time

### 11.5c: Vacuum / Maintenance Scheduling

- [ ] Per-model autovacuum tuning: `#[model(autovacuum = VacuumPolicy::HighChurn)]` emits per-table `ALTER TABLE ... SET (autovacuum_vacuum_scale_factor = ..., ...)` as DDL routed through Phase 7's migration generation pipeline. Phase 11.5 provides the policy vocabulary + CLI/ops surface; Phase 7 owns the DDL emission and phased execution
- [ ] `djogi ops vacuum --table <name> [--analyze] [--full]` — on-demand vacuum/analyze
- [ ] `djogi ops vacuum setup --weekly` — scheduled `VACUUM ANALYZE` across the schema, respecting autovacuum settings

### 11.5d: Health Checks

- [ ] `djogi ops doctor` — checks pool utilization, long-running transactions (> N seconds), table bloat estimates, index bloat, replication lag if configured, `pg_stat_statements` top-N slow queries
- [ ] Each check returns a pass/warn/fail with a suggested remediation

### 11.5e: Operator Runbooks

- [ ] Generate opinionated Markdown runbooks under `docs/ops/` covering: "my backup failed", "restore from last night", "I accidentally dropped a table", "vacuum is blocked"
- [ ] Runbooks reference the specific `djogi ops` commands that resolve each scenario

**Deliverable:** Djogi apps get production-grade ops (backups, PITR, vacuum, health, runbooks) without cobbling them together per project.

---

## Phase 12: Distributed Topology & Residency

**Goal:** Add descriptor-aware support for replicas, residency constraints, and topology-sensitive migration safety.

- [ ] Add explicit read-consistency modes such as primary-only, replica-allowed, read-your-writes, and stale-ok
- [ ] Add placement metadata for shard keys, residency classes, and relation placement constraints
- [ ] Validate topology-sensitive schema changes in migration tooling
- [ ] Extend repartition/partition tooling with topology-aware safety checks
- [ ] Keep deployment-specific routing implementations outside Djogi core

**Deliverable:** Djogi remains deployment-agnostic while providing the metadata, runtime contracts, and migration guardrails needed for distributed Postgres topologies.

---

## Phase 13: Runtime Query Verification (D7xx)

**Goal:** Three sequenced moves — **inventory** every generated SQL surface in djogi (Model CRUD, QuerySet, migration DDL, outbox templates, relation queries, FTS, spatial, auth) and classify each as static (finite template set) or dynamic (per-call construction); **introduce the `SqlSurface` trait** as the contract every emitter implements — static surfaces register their full shape list at startup, dynamic surfaces participate via a per-execution verification hook gated by an active-probe flag the verifier sets; **verify the registered shapes** with Postgres prepare — exhaustive for static surfaces, bounded-by-exercised-paths for dynamic. Detects the class of bug where descriptor and live DB schema agree (D6xx clean) but a SQL emitter has a bug that would cause runtime queries to fail or return mistyped data. Closes the projection-audit gap behind GH #133.

**The trait is the load-bearing design choice.** Without it, every new SQL-emitting surface is a "remember to register" footgun — silent coverage holes as djogi grows. With it, new surfaces either implement `SqlSurface` (covered automatically) or don't (explicitly uncovered, no false claim). Two coverage modes, one trait: `known_shapes()` for static (exhaustive at verify time); `is_dynamic()` + per-execution hook for dynamic (covers every shape that actually runs in verify-mode — test suite, staging traffic — but not shapes nobody hits). Coverage difference documented honestly in the adopter guide.

**Scope sibling — Phase 8.5 issue #152** covers the schema-level drift case (live DB diverges from descriptor → apply hard-fails via existing D6xx + new pre-flight gate). Phase 13 covers the orthogonal case: descriptor and DB agree, but a SQL emitter is wrong. Lower urgency, narrower surface, structurally invisible to integration tests that bypass the projection pipeline via `raw_*` (the GH #133 root cause).

Built natively on `tokio_postgres::Client::prepare`. No new external dependencies.

- [ ] **C0 — SQL-surface inventory.** Research deliverable. Walk djogi source, classify every emitter as static / parameterized / dynamic, output `docs/research/djogi-sql-surface-inventory.md`. **Gates everything else** — registration interface design depends on what surfaces it serves.
- [ ] **C1 — `SqlSurface` trait + `RuntimeQueryShape` + active-probe machinery + `FieldSqlType::expected_oid()` + extension-type OID lookup.** The shared mechanism: trait with `surface_name`, `known_shapes`, `is_dynamic`; `inventory::submit!` glue for static surfaces; process-wide active-probe flag + hook plumbing for dynamic surfaces; `expected_oid()` extends existing enum at `djogi/src/descriptor.rs:1394`; one-time `pg_type` lookup at verifier startup for `Geography{}` / `Citext` / `Custom(_)`. ~250-350 LOC.
- [ ] **C2 — Static surface impls (Model CRUD via macro + outbox + static relation queries + auth/FTS/spatial helpers).** Macro change in `djogi-macros/src/model/crud.rs` is the largest single piece (~200-500 LOC); other surfaces are manual `impl SqlSurface` blocks (~30-50 LOC each). Total ~300-700 LOC distributed. Highest-risk component.
- [ ] **C3 — Dynamic surface impl: QuerySet.** `impl SqlSurface for QuerySet { is_dynamic() = true }` plus per-execution prepare-check hook in `QuerySet::execute()` and other terminal methods, gated by the active-probe flag. Zero overhead when active-probe is off (production default); on during `djogi migrations verify --runtime` and opted-in test runs. ~150-250 LOC.
- [ ] **C4 — `migrate/verify_runtime.rs` + D7xx diagnostics.** Savepoint-protected probe: walk every registered `SqlSurface`, prepare-check `known_shapes()` for static, enable active-probe for dynamic during the verify pass, emit D7xx into existing `VerifyReport`, ROLLBACK. Reuses existing `VerifyDiagnostic` / `VerifySeverity` from `djogi/src/migrate/verify.rs`. ~300-500 LOC + integration tests covering both static and dynamic surfaces.
- [ ] **C5 — `--runtime` CLI flag + adopter docs.** Wire onto existing `djogi migrations verify`; companion adopter-facing guide `docs/spec/runtime-query-verification.md` explaining static-vs-dynamic coverage, when to enable verify-mode in tests, what's caught and what isn't.

**Reuse over reinvent (per 2026-05-09 survey):** `VerifyDiagnostic { code, severity, message, location }` from `verify.rs:179`, `VerifySeverity::{Info, Warning, Error}` from `verify.rs:161`, `VerifyReport.has_errors()` non-zero exit semantic, `FieldDescriptor.sql_type` (the type table djogi already has), `FieldDescriptor.nullable` (descriptor declares; no inference). All previously assumed to require new infrastructure; survey confirmed they exist and slot naturally.

**Cornucopia reference notes** *(`cornucopia-reference/` symlink, MIT/Apache-2.0, design reference only)*: After survey, cornucopia's relevance narrowed substantially. The `FieldSqlType` enum is djogi's analogue of cornucopia's `TypeRegistrar` — flat-table approach doesn't transfer because djogi's enum is richer and already exists. The lessons that DO transfer: nullability-by-declaration (already djogi's pattern via `Option<T>`), error-on-unknown-type with `col_name` + `col_ty` (D704). Detailed source review at `docs/research/cornucopia-type-mapping.md`.

**Deliverable:** `djogi migrations verify --runtime` detects projection-pipeline bugs across every SQL surface that implements `SqlSurface` — exhaustive for static surfaces, bounded-by-exercised-paths for dynamic. Coverage scope determined by the C0 inventory + which surfaces grow `SqlSurface` impls. Closes GH #133's audit gap without new dependencies, in roughly 1200-2300 LOC. The trait makes future SQL surfaces auto-covered (or explicitly uncovered), eliminating silent coverage holes as djogi grows. C2 (Model CRUD macro impl) and C3 (QuerySet dynamic hook) are the two highest-risk pieces.

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
| 5.5: Auth Substrate *(shipped)* | Medium | `DjogiAuth` trait, `AuthContext`, password hashing, auto-tenant scoping |
| 6: Spatial *(shipped)* | Medium | `GeoPoint`, PostGIS-backed radius/order-by-distance, GiST index metadata |
| 6.5: Spatial Polish | Medium | Non-point geometries, more predicates, bbox prefilter, test extensions arg |
| 7: Migrations | Large | Full migration system including online / zero-downtime patterns |
| 7.5: Protected Data | Medium | Sensitive-field metadata and codecs |
| 8: Hooks & Composition | Medium | Lifecycle hooks, abstract models, proxy, computed properties |
| 9: Shell, Analyzer & djqry | Medium | Interactive tools (admin console split out to Phase 10 / Maahi) |
| 9.5: Lifecycle | Medium | Governance and lifecycle planning (depends on 7.5) |
| 10: Maahi (Admin Console) | Large | Visage-RBAC, Dioxus full-stack, multi-tenancy, security floor, M2M with bulk threshold |
| 10.5: Maahi Compliance & Delegation | Medium | Multi-parent inheritance, manage_roles, broader approvals, Django parity |
| 11: Logging & Observability | Medium | Audit trail, tracing, slow-query hooks, metrics, admin views |
| 11.5: Ops Tooling | Medium | Turnkey backups, PITR, vacuum scheduling, health checks, runbooks |
| 12: Topology | Large | Residency, replica semantics, distributed guardrails |
| 13: Runtime Query Verification | Medium | Pre-flight prepare-protocol check that macro-generated SQL still type-checks post-migration; closes GH #133 projection-audit gap |

**The critical path to standing alongside popular Rust ORM alternatives is Phases 0–4.** Phase 4.5 improves contract hygiene and shared contract reuse without changing that write-path boundary. Phases 5–13 add the Postgres-native depth, governance, scale-oriented capabilities, and projection-correctness audit needed for broader high-scale confidence.
