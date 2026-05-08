# Djogi

*A Model-first, Postgres-native distributed data engine for Rust. Define your schema as structs; the ORM, migrations, audit trail, admin surface, shell bindings, and in-memory cache predicates all derive from one declaration. Your web framework of choice handles HTTP — Djogi owns the data tier end-to-end.*

---

Most Rust data libraries solve one slice — a query builder, or migrations, or an audit log. Djogi treats the data tier as the primary derivation target: every concern that touches your model definition (typed queries, schema evolution, change tracking, RLS, model projections via visages, in-memory cache, cross-runtime predicate algebra) lives in one integrated stack rather than a dozen disjoint crates you wire together yourself. Djogi still aims to do one thing well — that thing is data management.

Define your data schema as Rust structs, and Djogi derives the surrounding data machinery — ORM, migrations, admin UI, audit trail, shell bindings, JSONB schema handling. One definition, one data-layer derivation chain.

Djogi does not own routing, middleware, or rendering. The view layer belongs to whichever Rust web framework the adopter chooses — developers write ordinary handlers for that framework, not Djogi abstractions. Axum is the best-covered integration today (opt in with the `axum` feature flag), but Djogi's core is web-framework-agnostic: Warp, Actix, Rocket, Poem, or any other Rust web framework can drive Djogi models through their own per-framework feature flag or through manual wiring. The rendering layer is similarly the developer's choice — Dioxus, Askama, Maud, or whatever fits.

Djogi is therefore not a full application framework. It is a Postgres-native data layer and model runtime with optional tooling built on top.

**Design north star:** Define the model once, derive everything else — but make every derivation explicit and typed. No hidden middleware chains, no implicit magic, no convention-as-code. Typed and explicit, but never unnecessarily verbose.

**Performance contract:** Djogi must make efficient Postgres forms expressible in-framework for common production workloads. Raw SQL remains available, but it should be the escape hatch for unusual SQL shape — not the normal way to recover performance lost to the ORM.

**Repository test contract:** Integration tests in this repository must exercise Djogi's typed surface by default. Raw SQL, pool access, and direct driver calls are deliberate escape hatches that require the bypass harness documented in [`docs/spec/raw-sql-escape-hatches.md`](./docs/spec/raw-sql-escape-hatches.md).

**Public framework rule:** Djogi may be informed by demanding real-world applications, but this repository documents requirements only in product-agnostic systems language. Domain workflows, business policy, and app-specific integrations belong in application crates or separate companion crates, not in Djogi core.

**Postgres only.** Djogi targets PostgreSQL exclusively. This is not a temporary limitation — it is a permanent design decision. Postgres provides the features Djogi's derivation chain depends on: `JSONB` with path operators for typed schema fields, database-native ID generation via HeeRanjId (`generate_id()`, `heerid_next_desc()`, and siblings), advisory locks for migration safety, `RETURNING` clauses, row locks, rich indexing options, strong constraint semantics, and transactional DDL. Abstracting over multiple databases would mean giving up these capabilities or reimplementing them poorly. Every query Djogi generates, every migration it emits, every `Jsonb<T>` filter it compiles targets Postgres directly — no lowest-common-denominator SQL.

## Status

**Shipped** — usable today:

- **Phase 1** — `#[model]` proc macro, `Model` trait (`get` / `create` / `save` / `delete` / `refresh_from_db`), `ModelDescriptor` + `inventory` registration, typed field + PK injection, and deliberately gated raw-SQL escape hatches. See [the models guide](./docs/guide/models.md).
- **Phase 2** — lazy `QuerySet<T>`, typed `FieldRef<M, V>` lookups (`eq` / `gte` / `in_list` / `contains` / …), `Condition` tree + SQL emitter, terminal reads (`fetch_all` / `fetch_one` / `first` / `count` / `exists`), ordering + pagination + `DISTINCT` / `DISTINCT ON`, programmatic `{Model}Filter` + `filter_struct`, bulk `update` / `delete`. See [the queries guide](./docs/guide/queries.md).
- **Phase 3** — `ForeignKey<T>`, `OneToOneField<T>`, typed relation paths, explicit eager loading (`prefetch` / `select_related`), reverse accessor macros, and explicit-through `ManyToMany` support. See the [relations guide](./docs/guide/relations.md).
- **Phase 4** — `DjogiContext` + `atomic()` scope, savepoint nesting, `on_commit` callbacks, `save(&mut self)` with `RETURNING *` rehydration, transactional outbox (`#[model(events)]`), expression IR (`Expr<T>`: arithmetic, field-vs-field, CASE/WHEN, `EXISTS`, typed `OuterRef<M, V>` correlated subqueries), typed aggregates + annotations (`count`/`sum`/`avg`/`min`/`max` with `FILTER(WHERE)`), row locks (`select_for_update` / `nowait` / `skip_locked`), `DjogiError::LockConflict` + `is_transient` classification + `retry_on_conflict`, and write-path convenience (`get_or_create`, `update_or_create`, `in_bulk`, `bulk_create`, `bulk_update`, `bulk_upsert`, `create_or_find`, `bulk_upsert_by_descriptor`, scoped-sequence numbering via `#[field(sequence_within = "...")]`). See the [transactions guide](./docs/guide/transactions.md), [expressions guide](./docs/guide/expressions.md), and [outbox guide](./docs/guide/outbox.md).
- **Phase 4.5** — opt-in transport visages derived from every `#[model]` struct. `#[field(expose(public, self_view, admin, export))]` (scalar form) or `#[field(expose(public = "PeerProjection"))]` (relation form) generates four audience-specific Rust types per model (`{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`) with unconditional `serde::Serialize` / `Deserialize` derives. Default exposure is `internal` — absent annotation means the field does NOT appear in any transport visage. Scalar-only visages emit `impl From<&Model>`; relation-nesting visages emit `impl TryFrom<&Model>` with `Error = VisageError::UnresolvedRelation`. `FieldDescriptor.visage_map` is populated so later phases can consume visage metadata without re-parsing attributes. See the [visages guide](./docs/guide/visages.md).
- **Phase 5-Zero** — SQL substrate swap: `sqlx` and `heeranjid-sqlx` removed from the entire codebase. Runtime replaced by `tokio-postgres` + `postgres-types` + `deadpool-postgres`. `DjogiPool` wraps `deadpool_postgres::Pool`; `DjogiContext` dispatches over pool-backed and transaction-backed connections via `PgConnection`. `#[djogi_test]` harness uses pure `tokio_postgres` bootstrap with `heeranjid::postgres_schema::install_schema` + `seed_default_node`. `DjogiError::Sqlx` and its `sqlx::Error` payload give way to `DjogiError::Db` with the opaque `DbError` newtype; `SqlState`-based classification still feeds `is_transient()` / `is_lock_error()` / `retry_on_conflict`. Raw SQL remains available only through the hidden sealed bypass traits described in [raw SQL escape hatches](./docs/spec/raw-sql-escape-hatches.md). `FromPgRow` replaces `sqlx::FromRow` in every macro-emitted decode impl; `FromJoinedPgRow` handles prefix-keyed joined-row decode for `select_related`; `FromRowTuple` + `try_get_scalar` + `try_get_tuple` cover positional tuple decode for raw queries (arities 1..=8). All 96 `#[sqlx::test]` integration tests migrated to `#[djogi_test]`; per-test DB creation, HeeRanjID schema install, seed, and `heer.node_id` setup all handled by the macro's bootstrap.
- **Phase 5** — Postgres-native model layer on the tokio-postgres substrate: `Tracked<T>` explicit dirty-tracking wrapper with selective column writes; `#[field(version)]` optimistic locking with version predicate in every `save()` and `DjogiError::LockConflict` on conflict; `#[derive(DjogiEnum)]` typed Postgres enum codec + inventory metadata; `Jsonb<T>` with unknown-field preservation via `IndexMap<String, serde_json::Value>`, flat `.path::<V>("dot.path")` querying, and `#[derive(JsonbSchema)]` typed deep-path tree; `Vec<V>` array fields with `contains` / `contained_by` / `overlap` / `len` native operators; `#[model(tenant_key = "...")]` RLS policy DDL side-channel + `ctx.set_tenant(...)` + `_insecurely()` bypass methods; advisory rationale warnings emitted by `#[field(outbox)]` and other attributes at macro expansion; outbox worker primitives + `Publisher` trait + `NOTIFY` reference publisher; `QuerySet::stream` cursor-backed streaming terminal. See the [tracked fields](./docs/guide/tracked-fields.md), [optimistic locking](./docs/guide/optimistic-locking.md), [enums](./docs/guide/enums.md), [JSONB](./docs/guide/jsonb.md), [arrays](./docs/guide/arrays.md), and [tenancy](./docs/guide/tenancy.md) guides.

- **Phase 5.5** — Narrow authentication substrate on top of Phase 4's `DjogiContext` and Phase 5's tenant-keyed RLS. Pluggable `DjogiAuth` trait (not sealed — third-party providers first-class; object-safe so `Arc<dyn DjogiAuth>` works); value-typed `AuthContext` with `user_id: HeerId` / `tenant_id: Option<String>` / `scopes: Vec<String>` / `ext: HashMap<String, String>`; `DjogiContext::with_auth` consuming builder + `set_auth` mutating form for use inside `atomic()` closures; `PasswordHash` typed column with transparent `postgres_types::{ToSql, FromSql}` delegation + Argon2id hasher behind `auth-argon2` feature flag (constant-time `verify` returns `bool` to prevent timing leaks); automatic `set_tenant` integration — every CRUD / QuerySet op on a `#[model(tenant_key)]` model auto-issues `ensure_tenant_set(auth.tenant_id)` before execution, with per-tenant-id tracking (`applied_tenant_id: Option<String>`) that re-issues `SET LOCAL` when auth changes mid-transaction; grep-able `tracing::warn!` when `auth.tenant_id.is_none()` on a tenant-keyed model with `ctx.with_no_tenant_scope()` explicit opt-out for admin / batch flows; `_insecurely` bypass methods emit searchable warn logs with caller location via `#[track_caller]`; `AuthError` `#[non_exhaustive]` enum bridges to `DjogiError::Auth(AuthError)` via `#[from]`. See the [authentication guide](./docs/guide/auth.md).

- **Phase 6** — Typed spatial surface behind the `spatial` feature flag. `GeoPoint { lat, lon }` value type with coordinate validation (latitude in `-90.0..=90.0`, longitude in `-180.0..=180.0`), Haversine distance helper, WKT `Display` (`POINT(lon lat)` per OGC), and transparent `postgres_types::{ToSql, FromSql}` via a manual 25-byte EWKB codec for `GEOGRAPHY(Point, 4326)` — no new dependencies. Typed query surface on `FieldRef<M, GeoPoint>`: `within_km(center, km)` emits `ST_DWithin(col, ST_Point($lon, $lat)::geography, $meters)` and routes through `Condition::Expr` for IR uniformity with the Phase 4 expression substrate; `order_by_distance(center)` emits `ST_Distance(col, ST_Point($lon, $lat)::geography) ASC` with the primary-key column appended unconditionally as a deterministic tiebreak (pagination-safe even when rows are equidistant). `IndexSpec` gains `requires_out_of_transaction: bool` and `extension_dependency: Option<&'static str>`; `MigrationShape` contract helper proves the descriptor encodes sufficient information for Phase 7's migration emitter. Phase 6 locks SRID to 4326; non-4326 work goes through `ctx.raw_execute(...)` + `FieldSqlType::Custom`. See the [spatial guide](./docs/guide/spatial.md).

- **Phase 6.5** — Grouped aggregation as a default feature + spatial polish under the `spatial` flag. Default-feature grouping: three-stage type-state (`QuerySet<T>` → `GroupedQuerySet<T, K>` → `GroupedAnnotatedQuerySet<T, K, A>`) driven by sealed `IntoGroupKeyTuple` / `IntoAggregateTuple` traits across arities 1–4; `group_by` / `rollup` / `cube` / `group_by_sets` entry points; `.annotate(...)` emits window aggregates with `OVER ()` when called on an ungrouped queryset and plain `GROUP BY` aggregates when called on a grouped one; `AggregateExpr<V>::over(Window)` + `.distinct()` with illegal combinations caught as `DjogiError::UnsupportedAggregate`; synthetic `__djogi_agg_N` aliases with `DjogiError::AnnotationAliasCollision` on user collisions. Spatial polish: six `GeographyValue` types (`GeoPoint`, `LineString`, `Polygon`, `MultiPoint`, `MultiLineString`, `MultiPolygon`) with manual EWKB codecs and OGC simple-features validation; shape predicates on any `FieldRef<M, G: GeographyValue>` (`contains`, `intersects`, `touches`, `within`) with function-aware cast discipline (`ST_Intersects` keeps geography, the other three cast to `::geometry` + `$n::bytea::geometry` per PostGIS 3.x overload availability); `bounded_by` GiST-indexable bbox prefilter + `distance_to` first-class `Expr<f64>`; three spatial grouping entry points sharing a `SpatialGroupSource` enum (`group_by_region` / `count_by_region` using `ST_Covers` for a geography-native JOIN; `cluster_by_proximity` emitting `ST_ClusterDBSCAN(...) OVER ()` inside a subquery so the outer `GROUP BY cluster_id` bypasses the "window functions not allowed in GROUP BY" rule; `bucket_by_cell` for geohash precision bucketing); `#[djogi_test(extensions = [...])]` auto-provisions Postgres extensions per test with byte-level identifier validation (no regex). Zero new runtime dependencies. See the [query aggregation guide](./docs/guide/query-aggregation.md) and [spatial guide](./docs/guide/spatial.md).

- **Phase 7-Zero** — Indexing + apps substrate under the Phase 7 migration engine. `IndexSpec` v3 contract with `IndexKind::{NonUnique, UniqueConstraint, UniqueIndex}`, `IndexTarget::{Columns, Expression}`, `IndexColumnSpec { name, opclass, order, nulls }`, `predicate` (raw SQL) + `include` + `nulls_not_distinct` + `requires_out_of_transaction` + `extension_dependency`; five §6.4 escalation triggers unify on a `forces_unique_index` helper; `PkType::HeerIdDesc` + `PkType::RanjIdDesc` variants against heeranjid 0.3.0. Field-level `#[field(unique, index, index_method, nulls_not_distinct)]` grammar; model-level `#[model(indexes(index(...), unique(...), unique_index(...)))]` grammar with per-column `IndexColumnSpec`, expression targets, concurrently, partial/covering indexes. Deterministic `index_name` generation with `byte-level` identifier validation (no regex). Apps subsystem: `djogi::apps! { #[app(database = "…")] pub struct Vehicles; }` macro with type-path-valued `#[model(app = Vehicles)]`, convention-sealed `App` trait, `AppRegistry::all()` runtime registry (`(database, label)` identity, synthetic global bucket), lifecycle markers (`renamed_from` for rename, `tombstone` for retirement with compile-fail guard on active-model references) + `moved_from_app` historical metadata, `AppRegistry::cross_app_edges() / cross_app_cycles()` FK graph walker with `AppIdentity`, and the `AppDiagnostic` enum carrying D004 / D010 contracts for Phase 7 consumption. See the [apps guide](./docs/guide/apps.md).

- **Phase 7-Zero-2** — Visage query surface + PK refinements. **Default PK flip:** `#[model]` with no `pk` attribute now yields `HeerIdRecencyBiased` (newest-first index scans without a secondary descending index); built-ins re-exported under `djogi::types` as `HeerId` / `HeerIdRecencyBiased` / `RanjId` / `RanjIdRecencyBiased` (plus opt-in `Serial`). **PK trait split:** `PrimaryKey` (required, all PK kinds) + `PrimaryKeyDbGen` (DB-sourced, all built-ins + opt-in custom) + `PrimaryKeyClientGen` (client-side, custom-only). **Custom PK macro:** `djogi::primary_key! { pub struct ULID(uuid::Uuid); sql_type = "UUID"; default_sql = "gen_random_uuid()"; … }` — adopters integrate any ID scheme (UUIDv4, ULID, Snowflake, bespoke types) via a 4-line declaration + `#[model(pk = X)]`. **Ambient PK:** all built-in and custom PK kinds are usable in any field position, not just the PK slot. **`bulk_create` retrofit:** `PrimaryKeyDbGen::generate_many(ctx, n)` dispatched per `pk_kind` — one round-trip for N IDs, then INSERT with explicit `id` values. **Visage query surface (§5j):** every `{Model}Public` / `SelfView` / `Admin` / `Export` becomes a first-class query entity with its own `filter(...)` entry, `{Visage}Fields` accessor type, SELECT narrowing (only the visage's exposed columns), and compile-time FK / reverse-FK / M2M boundary enforcement. Reverse-FK accessors return `VisageQuerySet<ChildVisage>` with a typed FK predicate; M2M accessors return `VisageQuerySet<PeerVisage>` lowering to an `EXISTS (...)` correlated subquery against the through table (with table-qualified `OuterRef` to disambiguate framework-column collisions). New `expose(...)` grammar with `->` traversals: ID-only (no `->`), narrow peer (`-> Visage`), or full-struct (`-> ModelStruct`) optionally with nested `expose(...)`. `Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` lifted from Phase 4.5 deferral and project as `Option<PeerVisage>`. Visage querysets are read-only (mutations through the source model). `serde::{Serialize, Deserialize}` re-exported through `djogi::prelude::*` so adopter code does not need a separate `serde` line in `Cargo.toml` for `Jsonb<T>` payload schemas. See the [visages guide](./docs/guide/visages.md) and [primary keys spec](./docs/spec/primary-keys.md).

- **Phase 7** — Migration system. Schema differ, deterministic SQL emitter, runner with advisory-locked transactional / non-transactional segment dispatch, ledger (`djogi_schema_migrations`) with `V1:<sha256-hex>` checksum verification, `apply` / `rollback` / `fake` / `baseline` / `repair` / `verify` library APIs (CLI dispatch deferred to a Phase 7 follow-up — adopters can call the public entry points directly), `build.rs` drift diagnostics + per-bucket pending JSON staging. CLI registered today: `djogi migrations compose` + `status` + `attune` (`--record` / `--squash --from <ver>` with `--publish`), `djogi db reset` (triple-gated: localhost + non-production profile + explicit `--yes`; URL paths are percent-decoded and validated against the strict Postgres-identifier grammar before splicing into DDL — defence-in-depth against URL-injection), `djogi db seed` (`--database <name>` selects BOTH the seed directory and the per-database connection target via path-splice; idempotent `djogi_seed_runs` ledger + checksum-drift refusal + `--allow-non-localhost` opt-in), `djogi docs` (deterministic per-model Markdown reference pages from the descriptor inventory; field table includes a `Default` column populated from the PK strategy via the projection mirror). Filenames: `V<YYYYMMDDHHMMSS>__<slug>.sql` plus `.down.sql`. Per `(database, app)` bucket; cross-database FKs rejected at projection time. Out-of-order policy gates default to `Reject` on production / CI and `AllowWithDiagnostic` on dev. `attune --squash` localhost gate uses byte-level libpq + URL parsers (no regex). Every `db` / `migrations` subcommand obeys a uniform exit-code matrix: `0` success, `1` runtime error (config / network / SQL / replay), `2` refusal (policy gate OR clap-style argument validation) — bundling both refusal classes under `2` so CI scripts can treat any `2` as a soft "operator must intervene" skip without disambiguating gate-vs-arg. See the [migrations guide](./docs/guide/migrations.md) and [migrations spec](./docs/spec/migrations.md).

- **Phase 7.5** — Live-migration substrate + protected-data field attributes. `live_migrate::{plan, plan_file, state, classify}` substrate with `OnlineSafetyClassification` enum (`OnlineSafe` / `FastLockDestructiveGuarded` / `ExpandContract` / `OfflineOnly`, `#[non_exhaustive]`); `pg_volatility` introspection module; backfill engine with chunk-loop SQL pattern, plan resume, daemon-mode runner (T7+T8+T9). Operator surface (T10+T14+T15): `djogi live` CLI commands, spatial cfg gating, `db cleanup-test-dbs`, daemon-mode resume. `#[field(protect = "...")]` and related field-level RLS / audit attributes; `ProtectedFieldMetadata` descriptor surface; macro parsing via raw attribute walking. EXCLUSION + stored-generated descriptor extension: `ExclusionConstraintSpec`, `GeneratedColumnSpec`, `ExclusionElement` types; `ColumnSchema.generated`, `TableSchema.exclusion_constraints` snapshot fields with `#[serde(default)]` for back-compat; `SchemaOperation::{AddExclusionConstraint, DropExclusionConstraint}`; classifier routes EXCLUDE-on-populated and stored-generated-add to `OfflineOnly` (Pg18 has no NOT VALID for EXCLUDE); empty-table fast-path; macro cross-attribute validation rejects `generated` + `default` together with span-precise diagnostic. T13 (catalog drift detection / runtime-vs-spec divergence audit) deferred to Phase 8 or a dedicated bug-fix sprint.

**In flight / coming** — partially merged or design-locked, but not release-complete:

- **Phase 8** — 8α–8ζ implementation clusters are on `main`; 8η predicate/cache correctness and Phase 8.5 publish housekeeping remain before the v0.1.0 publish gate. See the [implementation plan](./docs/spec/implementation-plan.md).
- **Phase 9** — Rhai shell, analyzer, and model-aware operational tooling.
- Phase 10 — [Maahi](./docs/spec/maahi/index.md) admin console (Dioxus full-stack; visage-scope-driven RBAC; opt-in via `djogi = { features = ["admin"] }` which pulls in the `djogi-maahi` crate carve-out).

## What Djogi Owns

| Concern | Owner | Djogi's role |
|---|---|---|
| HTTP routing, middleware, request/response | Your Rust web framework of choice | Djogi does not own the request lifecycle. Integrations are adapters; Axum is the best-covered example today, and other frameworks are wired through adapters or manually. |
| DB driver, connection pooling, row mapping | `tokio-postgres` + `deadpool-postgres` + `postgres-types` | Djogi wraps the driver into a typed ORM layer (`Model`, `QuerySet`, `FromPgRow`, `ConditionBuilder`). Raw SQL and pool access remain available through the deliberate bypass harness. |
| ID generation | HeeRanjId | Calls the HeeRanjId generator family (`generate_id()`, `generate_ids(n)`, `heerid_next_desc()`, `generate_ids_desc(n)`, `generate_ranjid()`, `generate_ranjids_desc(n)`) and owns nothing else |
| Async runtime | Tokio | Uses it; does not configure it |
| Model ↔ DB mapping | **Djogi** | Proc macro, field types, QuerySet, relations |
| Migrations | **Djogi** | Differ, generation, snapshot management |
| Shell tooling | **Djogi** (opt-in) | Rhai REPL and model-aware operational tooling built on top of the core runtime |
| CRUD logging | **Djogi** | Audit trail, JSON-aware diffing |
| JSONB schema fields | **Djogi** | `Jsonb<T>` — typed schemas over JSONB columns |
| Admin tooling | **Djogi** (opt-in) | Auto-generated operational UI derived from `ModelDescriptor`; adapter surface, not Djogi's core identity |

## Core Dependencies

| Dependency | Role | Notes |
|---|---|---|
| `axum` | HTTP layer, routing, middleware | Opt-in via the `axum` feature flag; the best-covered framework integration today. Not a core dependency — Djogi's core runs without it. Other web frameworks integrate through their own per-framework feature flags or manual wiring. |
| `tokio-postgres` + `deadpool-postgres` + `postgres-types` | Async Postgres driver + connection pool + type codecs | Postgres 18+ only |
| `heeranjid` (postgres feature) | Distributed IDs + schema bootstrap | `HeerId` / `RanjId` codecs, the underlying desc variants used by Djogi's recency-biased PK surface, plus `install_schema` / `seed_default_node` helpers |
| `tokio` | Async runtime | Standard |
| `serde` / `serde_json` | Serialization | Model ↔ JSON for shell and API |
| `rhai` | Embedded scripting shell | Rust-adjacent syntax, sandboxable |
| `figment` | Layered configuration | env vars + `Djogi.toml` |
| `clap` | CLI subcommands | `cargo djogi migrate`, etc. |
| `inventory` | Compile-time app/model registration | Cross-crate model discovery |
| `syn` + `quote` + `proc-macro2` | Proc macro infrastructure | Powers `#[derive(Model)]` |
| `time` | Datetime field types | Preferred over chrono — cleaner API, no CVE history |
| `heeranjid` | Primary key generation | External crate — Djogi builds its public PK surface (`HeerIdRecencyBiased` default, `HeerId`, `RanjId`, `RanjIdRecencyBiased`) on top of HeeRanjId |

**Explicitly excluded:** SeaORM/SeaQuery, Diesel, `chrono`, random UUID (v4) as default PK.

## Project Layout

```
my-app/
  src/
    main.rs
    apps/
      vehicles/
        mod.rs              # djogi::register_app!(VehiclesApp)
        models.rs           # #[derive(Model)] structs + ManyToMany impls
        routes.rs           # web-framework handlers (Axum shown as the example; any Rust web framework works)
      people/
        mod.rs
        models.rs
        routes.rs
  build.rs                  # drift detection + migration generation
  Djogi.toml
  seeds.rhai
  migrations/               # git submodule — managed by pipeline
    schema_snapshot.json
    0001_initial_up.sql
    0001_initial_down.sql

djogi/                      # framework library crate
djogi-macros/               # proc macro crate (separate crate — required by Rust)
djogi-cli/                  # cargo djogi binary
djogi-shell/                # Rhai engine + model bindings
```

## Local Development

The `.scripts/` directory is a git submodule pointing to
[`TarunvirBains/scripts`](https://github.com/TarunvirBains/scripts). Clone
with submodules to pick it up:

```bash
git clone --recurse-submodules https://github.com/TarunvirBains/djogi.git
# or, if you already cloned:
git submodule update --init --recursive
```

Reproduce CI locally with:

```bash
./.scripts/ci-local.sh
```

## Specification

The full framework specification lives in [`docs/spec/`](./docs/spec/index.md):

| Doc | Covers |
|---|---|
| [Models & Field System](./docs/spec/models.md) | `#[derive(Model)]`, field types, annotations, dirty tracking |
| [Architecture Principles](./docs/spec/architecture-principles.md) | Public requirement translation, single-responsibility, framework boundaries |
| [Query API](./docs/spec/queries.md) | QuerySet, conditions, programmatic filters, ConditionBuilder |
| [JSONB Schema Fields](./docs/spec/jsonb.md) | `Jsonb<T>`, unknown field preservation, validation, subfield queries |
| [Relations](./docs/spec/relations.md) | ForeignKey, ManyToMany, explicit through models |
| [Primary Keys](./docs/spec/primary-keys.md) | Djogi PK surface: `HeerIdRecencyBiased` default, ascending `HeerId`, `RanjId`, `RanjIdRecencyBiased`, generation patterns |
| [Migrations](./docs/spec/migrations.md) | Build-time drift detection, schema snapshots, differ |
| [Logging](./docs/spec/logging.md) | Three-database architecture, CRUD audit trail, event tracing |
| [Configuration & CLI](./docs/spec/configuration.md) | `Djogi.toml`, `cargo djogi`, app registration, and web framework integration (`axum` shown as the concrete example) |
| [Shell](./docs/spec/shell.md) | Rhai REPL, transactions, import/export, seed scripts |
| [Maahi (Admin Console)](./docs/spec/maahi/index.md) | Dioxus full-stack admin with visage-scope-driven RBAC, multi-tenancy, six-action permissions, M2M inlines, and the inline-bulk approval threshold |
| [Scope & Boundaries](./docs/spec/scope.md) | What belongs in Djogi vs an app crate or companion crate |
| [Research Areas](./docs/spec/research.md) | Open implementation questions by subsystem |
| [Raw SQL Escape Hatches](./docs/spec/raw-sql-escape-hatches.md) | Raw SQL as djogi's `unsafe`, bypass attribute, justification comments, and pin-test policy |
| [Design Decisions](./docs/spec/decisions.md) | Full decision log |

## Background

- [The Agentic Shift](./docs/hypothesis.md) — why Model-first frameworks exist
