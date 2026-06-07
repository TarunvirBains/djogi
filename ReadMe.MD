# Djogi

*A Model-first, Postgres-native distributed data engine for Rust. Define your schema as structs; the ORM, migrations, audit trail, planned admin surface, planned shell bindings, and in-memory cache predicates all derive from one declaration. Your web framework of choice handles HTTP — Djogi owns the data tier end-to-end.*

---

Most Rust data libraries solve one slice — a query builder, or migrations, or an audit log. Djogi treats the data tier as the primary derivation target: every concern that touches your model definition (typed queries, schema evolution, change tracking, RLS, model projections via visages, in-memory cache, cross-runtime predicate algebra) lives in one integrated stack rather than a dozen disjoint crates you wire together yourself. Djogi still aims to do one thing well — that thing is data management.

Define your data schema as Rust structs, and Djogi derives the surrounding data machinery — ORM, migrations, audit trail, JSONB schema handling, with admin UI and shell bindings planned as later opt-in surfaces. One definition, one data-layer derivation chain.

Djogi does not own routing, middleware, or rendering. The view layer belongs to whichever Rust web framework the adopter chooses — developers write ordinary handlers for that framework, not Djogi abstractions. Axum is the best-covered integration today (opt in with the `axum` feature flag), but Djogi's core is web-framework-agnostic: Warp, Actix, Rocket, Poem, or any other Rust web framework can drive Djogi models through their own per-framework feature flag or through manual wiring. The rendering layer is similarly the developer's choice — Dioxus, Askama, Maud, or whatever fits.

Djogi is therefore not a full application framework. It is a Postgres-native data layer and model runtime with optional tooling built on top.

**Design north star:** Define the model once, derive everything else — but make every derivation explicit and typed. No hidden middleware chains, no implicit magic, no convention-as-code. Typed and explicit, but never unnecessarily verbose.

**Performance contract:** Djogi must make efficient Postgres forms expressible in-framework for common production workloads. Raw SQL remains available, but it should be the escape hatch for unusual SQL shape — not the normal way to recover performance lost to the ORM.

**Repository test contract:** Integration tests in this repository must exercise Djogi's typed surface by default. Raw SQL, pool access, and direct driver calls are deliberate escape hatches that require the bypass harness documented in [`docs/spec/raw-sql-escape-hatches.md`](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).

**Public framework rule:** Djogi may be informed by demanding real-world applications, but this repository documents requirements only in product-agnostic systems language. Domain workflows, business policy, and app-specific integrations belong in application crates or separate companion crates, not in Djogi core.

**Postgres only.** Djogi targets PostgreSQL exclusively. This is not a temporary limitation — it is a permanent design decision. Postgres provides the features Djogi's derivation chain depends on: `JSONB` with path operators for typed schema fields, database-native ID generation via HeeRanjId (`heerid_next()`, `heerid_next_desc()`, `ranjid_next()`, `ranjid_next_desc()`, and batch siblings), advisory locks for migration safety, `RETURNING` clauses, row locks, rich indexing options, strong constraint semantics, and transactional DDL. Abstracting over multiple databases would mean giving up these capabilities or reimplementing them poorly. Every query Djogi generates, every migration it emits, every `Jsonb<T>` filter it compiles targets Postgres directly — no lowest-common-denominator SQL.

## Status

**Current release:** `v0.1.0-alpha.9` (public alpha; API may change before v0.1.0 stable)

**Shipped & usable today:**

- **Models & schema derivation** — `#[derive(Model)]` proc macro generates the `Model` trait with typed CRUD operations (`create` / `get` / `save` / `delete` / `refresh_from_db`), injects `id` and `created_at` / `updated_at` fields, registers models via `inventory` for app discovery, and surfaces a `ModelDescriptor` for migrations and tooling. Raw SQL escape hatches remain available through a deliberate opt-in bypass harness. See the [models guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/models.md).

- **Typed queries** — `QuerySet<T>` is a lazy query builder with typed `FieldRef<M, V>` lookups (`eq`, `gte`, `in_list`, `contains`, etc.), a `Condition` tree that compiles to SQL, and terminal methods (`fetch_all`, `fetch_one`, `first`, `count`, `exists`, `stream`). Queries support ordering, pagination, `DISTINCT` / `DISTINCT ON`, programmatic filters via `{Model}Filter`, and bulk operations (`update`, `delete`). See the [queries guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/queries.md).

- **Relations** — `ForeignKey<T>` and `OneToOneField<T>` for one-to-many and one-to-one relationships; explicit many-to-many support via `ManyToMany` with required through models. Eager loading via `prefetch` and `select_related` with reverse accessors. See the [relations guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/relations.md).

- **Transactions & outbox** — `DjogiContext` scopes database operations with `atomic()`, nesting savepoints. `on_commit` callbacks fire after transaction commit. Transactional outbox (`#[model(events)]`) publishes events durably via a `Publisher` trait with a `NOTIFY`-based reference implementation. See the [transactions](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/transactions.md) and [outbox](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/outbox.md) guides.

- **Expressions & aggregates** — `Expr<T>` models SQL expressions: arithmetic, field-vs-field comparisons, `CASE`/`WHEN`, `EXISTS` subqueries, and correlated subqueries via `OuterRef<M, V>`. Typed aggregates (`count`, `sum`, `avg`, `min`, `max`) support `FILTER(WHERE)` clauses. Row locks (`select_for_update`, `nowait`, `skip_locked`) and write-path helpers (`get_or_create`, `update_or_create`, `in_bulk`, `bulk_create`, `bulk_update`, `bulk_upsert`, `create_or_find`). Scoped sequence numbering via `#[field(sequence_within = "...")]`. See the [expressions](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/expressions.md) guide.

- **Query aggregation** — Group results by one or more fields with `group_by`, `rollup`, `cube`, and `group_by_sets`. Annotations emit window aggregates (`OVER ()`) on ungrouped queries and `GROUP BY` aggregates on grouped queries. Illegal combinations detected at compile time. See the [aggregation guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/query-aggregation.md).

- **Typed column types** — `Jsonb<T>` wraps JSONB columns with a typed schema, preserving unknown fields, and supports flat dot-path queries. `#[derive(DjogiEnum)]` generates typesafe Postgres enum codecs. `Vec<V>` array fields with native operators (`contains`, `contained_by`, `overlap`, `len`). `Tracked<T>` for explicit dirty-tracking with selective column writes. `#[field(version)]` optimistic locking with version predicates. See the [JSONB](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/jsonb.md), [enums](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/enums.md), [arrays](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/arrays.md), [tracked fields](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/tracked-fields.md), and [optimistic locking](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/optimistic-locking.md) guides.

- **Auth & multi-tenancy** — Pluggable `DjogiAuth` trait for custom authentication providers. `AuthContext` carries `user_id`, `tenant_id`, `scopes`, and extension data. `PasswordHash` fields with Argon2id hashing behind the `auth-argon2` feature flag. `#[model(tenant_key = "...")]` enforces row-level security with `set_tenant()` integration. See the [authentication](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/auth.md) and [tenancy](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/tenancy.md) guides.

- **Transport visages** — `#[field(expose(...))]` generates audience-specific projection types (`Public`, `SelfView`, `Admin`, `Export`) from every model, with unconditional serde derives. Scalar-only visages emit `impl From<&Model>`; relation visages emit `impl TryFrom<&Model>`. Visage querysets provide read-only filtered access with compile-time relation boundary enforcement. See the [visages guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/visages.md).

- **Spatial queries** (behind the `spatial` feature flag) — `GeoPoint` and five geography types (`LineString`, `Polygon`, `MultiPoint`, `MultiLineString`, `MultiPolygon`) with OGC validation and manual EWKB codecs. Typed predicates on `FieldRef<M, G>`: `within_km`, `contains`, `intersects`, `touches`, `within`, plus `order_by_distance` for pagination-safe nearest-neighbor queries. Spatial grouping via `group_by_region`, `cluster_by_proximity` (DBSCAN), and `bucket_by_cell` (geohash). See the [spatial guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/spatial.md).

- **Indexes & constraints** — `#[field(unique, index, index_method, nulls_not_distinct)]` and model-level `#[model(indexes(...))]` define traditional and expression indexes, partial/covering indexes, and constraint enforcement. Deterministic index naming with byte-level identifier validation.

- **Multi-app support** — The `djogi::apps!` macro registers multiple logical apps (separate databases, shared Postgres cluster). Per-app migrations with cross-app FK validation. See the [apps guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/apps.md).

- **Primary keys** — Four built-in PK kinds: `HeerIdRecencyBiased` (default, newest-first 64-bit), `HeerId` (ascending 64-bit), `RanjId` (128-bit UUIDv8), `RanjIdRecencyBiased`. Custom PK macros (`djogi::primary_key!`) integrate any ID scheme (UUIDv4, ULID, Snowflake). Bulk PK generation via `PrimaryKeyDbGen::generate_many(ctx, n)` with one round-trip. See the [primary keys spec](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/primary-keys.md).

- **Migrations** — Build-time schema differ and deterministic SQL emitter. Advisory-locked runner with transactional and non-transactional segment dispatch. Checksummed ledger (`djogi_schema_migrations`) with `apply` / `verify` / `repair` / `baseline` / `fake` library APIs. CLI: `djogi migrations apply` (+ `--fake` / `--reason` flags), `compose`, `status`, `verify`, `repair`, `baseline`, `attune` (with `--squash` / `--record` / `--publish` for offline work). `djogi db reset` (triple-gated for safety), `djogi db seed`, and `djogi docs` (auto-generate Markdown reference). Crash recovery and existing-database adoption workflows documented. See the [migrations guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/migrations.md) and [migrations spec](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/migrations.md).

- **Live-migration infrastructure** — Substrate for safe online schema changes. `OnlineSafetyClassification` enum routes changes to transactional fast-path or offline-only. Protected-field attributes (`#[field(protect = "...")]`) for audit and RLS. Exclusion constraints and generated columns supported. (CLI commands `djogi live ...` are not yet released in v0.1.0-alpha.9.)

**Planned — not yet shipped:**

- **Rhai shell** — Interactive REPL for model inspection, query building, and transaction control. Planned for a future release.
- **Maahi admin console** — Dioxus full-stack operations UI derived from `ModelDescriptor`, with visage-scope-driven RBAC and multi-tenant support. Planned opt-in via `djogi = { features = ["admin"] }`.

## What Djogi Owns

| Concern | Owner | Djogi's role |
|---|---|---|
| HTTP routing, middleware, request/response | Your Rust web framework of choice | Djogi does not own the request lifecycle. Integrations are adapters; Axum is the best-covered example today, and other frameworks are wired through adapters or manually. |
| DB driver, connection pooling, row mapping | `tokio-postgres` + `deadpool-postgres` + `postgres-types` | Djogi wraps the driver into a typed ORM layer (`Model`, `QuerySet`, `FromPgRow`, `ConditionBuilder`). Raw SQL and pool access remain available through the deliberate bypass harness. |
| ID generation | HeeRanjId | Calls the HeeRanjId generator family (`heerid_next()`, `heerid_next_desc()`, `ranjid_next()`, `ranjid_next_desc()`, plus `generate_ids(...)` / `generate_ranjids(...)` batch helpers) and owns nothing else |
| Async runtime | Tokio | Uses it; does not configure it |
| Model ↔ DB mapping | **Djogi** | Proc macro, field types, QuerySet, relations |
| Migrations | **Djogi** | Differ, generation, snapshot management |
| Shell tooling | **Djogi** (planned) | Rhai REPL and model-aware operational tooling |
| CRUD logging | **Djogi** | Audit trail, JSON-aware diffing |
| JSONB schema fields | **Djogi** | `Jsonb<T>` — typed schemas over JSONB columns |
| Admin tooling | **Djogi** (planned opt-in) | Maahi operational UI derived from `ModelDescriptor` |

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
| `clap` | CLI subcommands | `djogi migrations apply/compose/status/attune`, `djogi db reset/seed`, `djogi docs`, and related shipped subcommands |
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
        mod.rs              # djogi::apps! { ... }
        models.rs           # #[derive(Model)] structs + relations
        routes.rs           # web-framework handlers (Axum shown as the example; any Rust web framework works)
      people/
        mod.rs
        models.rs
        routes.rs
  build.rs                  # drift detection + migration generation
  Djogi.toml
  seeds/
    default/                # *.sql files executed via djogi db seed
    replica/                # per-database seed files
  migrations/               # git submodule — managed by pipeline
    schema_snapshot.json
    V20260531120000__initial.sdjql
    V20260531120000__initial.down.sdjql

djogi/                      # framework library crate
djogi-macros/               # proc macro crate (separate crate — required by Rust)
djogi-cli/                  # standalone `djogi` binary
djogi-shell/                # Rhai engine + model bindings (planned)
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

The full framework specification lives in [`docs/spec/`](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/index.md):

| Doc | Covers |
|---|---|
| [Models & Field System](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/models.md) | `#[derive(Model)]`, field types, annotations, dirty tracking |
| [Architecture Principles](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/architecture-principles.md) | Public requirement translation, single-responsibility, framework boundaries |
| [Query API](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/queries.md) | QuerySet, conditions, programmatic filters, ConditionBuilder |
| [JSONB Schema Fields](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/jsonb.md) | `Jsonb<T>`, unknown field preservation, validation, subfield queries |
| [Relations](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/relations.md) | ForeignKey, ManyToMany, explicit through models |
| [Primary Keys](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/primary-keys.md) | Djogi PK surface: `HeerIdRecencyBiased` default, ascending `HeerId`, `RanjId`, `RanjIdRecencyBiased`, generation patterns |
| [Migrations](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/migrations.md) | Build-time drift detection, schema snapshots, differ |
| [Logging](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/logging.md) | Three-database architecture, CRUD audit trail, event tracing |
| [Configuration & CLI](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/configuration.md) | `Djogi.toml`, the `djogi` CLI, app registration, and web framework integration (`axum` shown as the concrete example) |
| [Shell](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/shell.md) | Rhai REPL, transactions, import/export, seed scripts |
| [Maahi (Admin Console)](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/maahi/index.md) | Dioxus full-stack admin with visage-scope-driven RBAC, multi-tenancy, six-action permissions, M2M inlines, and the inline-bulk approval threshold |
| [Scope & Boundaries](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/scope.md) | What belongs in Djogi vs an app crate or companion crate |
| [Research Areas](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/research.md) | Open implementation questions by subsystem |
| [Raw SQL Escape Hatches](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md) | Raw SQL as djogi's `unsafe`, bypass attribute, justification comments, and pin-test policy |
| [Design Decisions](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/decisions.md) | Full decision log |

## Background

- [The Agentic Shift](https://github.com/TarunvirBains/djogi/blob/main/docs/hypothesis.md) — why Model-first frameworks exist
