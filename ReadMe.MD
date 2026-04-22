# Djogi

*A Model-first, Postgres-native distributed data engine for Rust. Define your schema as structs — derive ORM, migrations, audit trails, admin UI, and shell bindings. Your web framework of choice handles HTTP; Djogi handles data.*

---

Define your data schema as Rust structs, and Djogi derives the surrounding data machinery — ORM, migrations, admin UI, audit trail, shell bindings, JSONB schema handling. One definition, one data-layer derivation chain.

Djogi does not own routing, middleware, or rendering. The view layer belongs to whichever Rust web framework the adopter chooses — developers write ordinary handlers for that framework, not Djogi abstractions. Axum is the best-covered integration today (opt in with the `axum` feature flag), but Djogi's core is web-framework-agnostic: Warp, Actix, Rocket, Poem, or any other Rust web framework can drive Djogi models through their own per-framework feature flag or through manual wiring. The rendering layer is similarly the developer's choice — Dioxus, Askama, Maud, or whatever fits.

Djogi is therefore not a full application framework. It is a Postgres-native data layer and model runtime with optional tooling built on top.

**Design north star:** Define the model once, derive everything else — but make every derivation explicit and typed. No hidden middleware chains, no implicit magic, no convention-as-code. Typed and explicit, but never unnecessarily verbose.

**Performance contract:** Djogi must make efficient Postgres forms expressible in-framework for common production workloads. Raw SQL remains available, but it should be the escape hatch for unusual SQL shape — not the normal way to recover performance lost to the ORM.

**Public framework rule:** Djogi may be informed by demanding real-world applications, but this repository documents requirements only in product-agnostic systems language. Domain workflows, business policy, and app-specific integrations belong in application crates or separate companion crates, not in Djogi core.

**Postgres only.** Djogi targets PostgreSQL exclusively. This is not a temporary limitation — it is a permanent design decision. Postgres provides the features Djogi's derivation chain depends on: `JSONB` with path operators for typed schema fields, database-native ID generation via `generate_id()` (HeeRanjId), advisory locks for migration safety, `RETURNING` clauses, row locks, rich indexing options, strong constraint semantics, and transactional DDL. Abstracting over multiple databases would mean giving up these capabilities or reimplementing them poorly. Every query Djogi generates, every migration it emits, every `Jsonb<T>` filter it compiles targets Postgres directly — no lowest-common-denominator SQL.

## Status

**Shipped** — usable today:

- **Phase 1** — `#[model]` proc macro, `Model` trait (`get` / `create` / `save` / `delete` / `refresh_from_db`), `ModelDescriptor` + `inventory` registration, typed field + PK injection, `djogi::raw::*` escape hatches. See [the models guide](./docs/guide/models.md).
- **Phase 2** — lazy `QuerySet<T>`, typed `FieldRef<M, V>` lookups (`eq` / `gte` / `in_list` / `contains` / …), `Condition` tree + SQL emitter, terminal reads (`fetch_all` / `fetch_one` / `first` / `count` / `exists`), ordering + pagination + `DISTINCT` / `DISTINCT ON`, programmatic `{Model}Filter` + `filter_struct`, bulk `update` / `delete`. See [the queries guide](./docs/guide/queries.md).
- **Phase 3** — `ForeignKey<T>`, `OneToOneField<T>`, typed relation paths, explicit eager loading (`prefetch` / `select_related`), reverse accessor macros, and explicit-through `ManyToMany` support. See the [relations guide](./docs/guide/relations.md).
- **Phase 4** — `DjogiContext` + `atomic()` scope, savepoint nesting, `on_commit` callbacks, `save(&mut self)` with `RETURNING *` rehydration, transactional outbox (`#[model(events)]`), expression IR (`Expr<T>`: arithmetic, field-vs-field, CASE/WHEN, `EXISTS`, typed `OuterRef<M, V>` correlated subqueries), typed aggregates + annotations (`count`/`sum`/`avg`/`min`/`max` with `FILTER(WHERE)`), row locks (`select_for_update` / `nowait` / `skip_locked`), `DjogiError::LockConflict` + `is_transient` classification + `retry_on_conflict`, and write-path convenience (`get_or_create`, `update_or_create`, `in_bulk`, `bulk_create`, `bulk_update`, `bulk_upsert`, `create_or_find`, `bulk_upsert_by_descriptor`, scoped-sequence numbering via `#[field(sequence_within = "...")]`). See the [transactions guide](./docs/guide/transactions.md), [expressions guide](./docs/guide/expressions.md), and [outbox guide](./docs/guide/outbox.md).
- **Phase 4.5** — opt-in transport visages derived from every `#[model]` struct. `#[field(expose(public, self_view, admin, export))]` (scalar form) or `#[field(expose(public = "PeerProjection"))]` (relation form) generates four audience-specific Rust types per model (`{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`) with unconditional `serde::Serialize` / `Deserialize` derives. Default exposure is `internal` — absent annotation means the field does NOT appear in any transport visage. Scalar-only visages emit `impl From<&Model>`; relation-nesting visages emit `impl TryFrom<&Model>` with `Error = VisageError::UnresolvedRelation`. `FieldDescriptor.visage_map` is populated so later phases can consume visage metadata without re-parsing attributes. See the [visages guide](./docs/guide/visages.md).
- **Phase 5-Zero** — SQL substrate swap: `sqlx` and `heeranjid-sqlx` removed from the entire codebase. Runtime replaced by `tokio-postgres` + `postgres-types` + `deadpool-postgres`. `DjogiPool` wraps `deadpool_postgres::Pool`; `DjogiContext` dispatches over pool-backed and transaction-backed connections via `PgConnection`. `#[djogi_test]` harness uses pure `tokio_postgres` bootstrap with `heeranjid::postgres_schema::install_schema` + `seed_default_node`. `DjogiError::Sqlx` and its `sqlx::Error` payload give way to `DjogiError::Db` with the opaque `DbError` newtype; `SqlState`-based classification still feeds `is_transient()` / `is_lock_error()` / `retry_on_conflict`. Raw-SQL surface on `DjogiContext`: `raw_query<T: FromPgRow>`, `raw_fetch_one<T: FromPgRow>`, `raw_scalar<T: FromSql>`, `raw_execute` — all transaction-aware, all taking positional `$n` binds as `&[&(dyn ToSql + Sync)]`. `FromPgRow` replaces `sqlx::FromRow` in every macro-emitted decode impl; `FromJoinedPgRow` handles prefix-keyed joined-row decode for `select_related`; `FromRowTuple` + `try_get_scalar` + `try_get_tuple` cover positional tuple decode for raw queries (arities 1..=8). All 96 `#[sqlx::test]` integration tests migrated to `#[djogi_test]`; per-test DB creation, HeeRanjID schema install, seed, and `heer.node_id` setup all handled by the macro's bootstrap.
- **Phase 5** — Postgres-native model layer on the tokio-postgres substrate: `Tracked<T>` explicit dirty-tracking wrapper with selective column writes; `#[field(version)]` optimistic locking with version predicate in every `save()` and `DjogiError::LockConflict` on conflict; `#[derive(DjogiEnum)]` typed Postgres enum codec + inventory metadata; `Jsonb<T>` with unknown-field preservation via `IndexMap<String, serde_json::Value>`, flat `.path::<V>("dot.path")` querying, and `#[derive(JsonbSchema)]` typed deep-path tree; `Vec<V>` array fields with `contains` / `contained_by` / `overlap` / `len` native operators; `#[model(tenant_key = "...")]` RLS policy DDL side-channel + `ctx.set_tenant(...)` + `_insecurely()` bypass methods; `LISTEN/NOTIFY` outbox delivery backend; `QuerySet::stream` cursor-backed streaming terminal. See the [tracked fields](./docs/guide/tracked-fields.md), [optimistic locking](./docs/guide/optimistic-locking.md), [enums](./docs/guide/enums.md), [JSONB](./docs/guide/jsonb.md), [arrays](./docs/guide/arrays.md), and [tenancy](./docs/guide/tenancy.md) guides.

**Coming** — design-locked but not yet implemented:

- Phase 5.5 — `DjogiAuth` trait, `EnvAuth` default implementation, Axum middleware that calls `ctx.set_tenant(...)` on request entry.
- Phase 6+ — `cargo djogi migrate`, Rhai shell, admin panel.

## What Djogi Owns

| Concern | Owner | Djogi's role |
|---|---|---|
| HTTP routing, middleware, request/response | Your Rust web framework of choice | Djogi does not own the request lifecycle. Integrations are adapters; Axum is the best-covered example today, and other frameworks are wired through adapters or manually. |
| DB driver, connection pooling, row mapping | `tokio-postgres` + `deadpool-postgres` + `postgres-types` | Djogi wraps the driver into a typed ORM layer (`Model`, `QuerySet`, `FromPgRow`, `ConditionBuilder`) but never hides it — `ctx.raw_execute` / `ctx.raw_scalar` escape hatches remain available |
| ID generation | HeeRanjId | Calls `generate_id()` / `generate_ids(n)` / `generate_ranj_id()` — owns nothing else |
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
| `heeranjid` (postgres feature) | Distributed IDs + schema bootstrap | `HeerId` / `RanjId` codecs + `install_schema` / `seed_default_node` helpers |
| `tokio` | Async runtime | Standard |
| `serde` / `serde_json` | Serialization | Model ↔ JSON for shell and API |
| `rhai` | Embedded scripting shell | Rust-adjacent syntax, sandboxable |
| `figment` | Layered configuration | env vars + `Djogi.toml` |
| `clap` | CLI subcommands | `cargo djogi migrate`, etc. |
| `inventory` | Compile-time app/model registration | Cross-crate model discovery |
| `syn` + `quote` + `proc-macro2` | Proc macro infrastructure | Powers `#[derive(Model)]` |
| `time` | Datetime field types | Preferred over chrono — cleaner API, no CVE history |
| `heeranjid` | Primary key generation | External crate — HeerId (64-bit) and RanjId (128-bit UUIDv8) time-ordered IDs |

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
git clone --recurse-submodules git@github.com:TarunvirBains/djogi.git
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
| [Primary Keys](./docs/spec/primary-keys.md) | HeeRanjId: HeerId (64-bit), RanjId (128-bit UUIDv8), generation patterns |
| [Migrations](./docs/spec/migrations.md) | Build-time drift detection, schema snapshots, differ |
| [Logging](./docs/spec/logging.md) | Three-database architecture, CRUD audit trail, event tracing |
| [Configuration & CLI](./docs/spec/configuration.md) | `Djogi.toml`, `cargo djogi`, app registration, and web framework integration (`axum` shown as the concrete example) |
| [Shell](./docs/spec/shell.md) | Rhai REPL, transactions, import/export, seed scripts |
| [Admin Panel](./docs/spec/admin.md) | Dioxus ModelForms, list views, validation, M2M inlines |
| [Scope & Boundaries](./docs/spec/scope.md) | What belongs in Djogi vs an app crate or companion crate |
| [Research Areas](./docs/spec/research.md) | Open implementation questions by subsystem |
| [Design Decisions](./docs/spec/decisions.md) | Full decision log |

## Background

- [The Agentic Shift](./docs/hypothesis.md) — why Model-first frameworks exist
