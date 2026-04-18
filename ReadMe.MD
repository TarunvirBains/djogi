# Djogi

*A Model-first, Postgres-native distributed data engine for Rust. Define your schema as structs — derive ORM, migrations, audit trails, admin UI, and shell bindings. Axum handles HTTP; Djogi handles data.*

---

Define your data schema as Rust structs, and the framework derives everything else — ORM, migrations, admin UI, audit trail, shell bindings, JSONB schema handling. One definition, full derivation chain.

Djogi does not own routing, middleware, or rendering. The View layer belongs to Axum — developers write standard Axum handlers, not Djogi abstractions. The rendering layer is the developer's choice — Dioxus, Askama, Maud, or whatever fits. Djogi merges app routers at startup and gets out of the way.

**Design north star:** Define the model once, derive everything else — but make every derivation explicit and typed. No hidden middleware chains, no implicit magic, no convention-as-code. Typed and explicit, but never unnecessarily verbose.

**Postgres only.** Djogi targets PostgreSQL exclusively. This is not a temporary limitation — it is a permanent design decision. Postgres provides the features Djogi's derivation chain depends on: `JSONB` with path operators for typed schema fields, database-native ID generation via `generate_id()` (HeeRanjId), advisory locks for migration safety, `RETURNING` clauses, rich constraint system, and transactional DDL. Abstracting over multiple databases would mean giving up these capabilities or reimplementing them poorly. Every query Djogi generates, every migration it emits, every `Jsonb<T>` filter it compiles targets Postgres directly — no lowest-common-denominator SQL.

## Status

**Shipped** — usable today:

- **Phase 1** — `#[model]` proc macro, `Model` trait (`get` / `create` / `save` / `delete` / `refresh_from_db`), `ModelDescriptor` + `inventory` registration, typed field + PK injection, `djogi::raw::*` escape hatches. See [the models guide](./docs/guide/models.md).
- **Phase 2** — lazy `QuerySet<T>`, typed `FieldRef<M, V>` lookups (`eq` / `gte` / `in_list` / `contains` / …), `Condition` tree + SQL emitter, terminal reads (`fetch_all` / `fetch_one` / `first` / `count` / `exists`), ordering + pagination + `DISTINCT` / `DISTINCT ON`, programmatic `{Model}Filter` + `filter_struct`, bulk `update` / `delete`. See [the queries guide](./docs/guide/queries.md).

**Coming** — design-locked but not yet implemented:

- Phase 3 — `ForeignKey<T>`, `ManyToMany`, explicit through models.
- Phase 4 — transactions-as-types, expression layer (`col = col + 1`).
- Phase 5 — Postgres-native: partitioning, RLS / `TenantScoped<T>`, Jsonb path ops.
- Phase 6+ — `cargo djogi migrate`, Rhai shell, admin panel.

## What Djogi Owns

| Concern | Owner | Djogi's role |
|---|---|---|
| HTTP routing, middleware, request/response | Axum | Wires registered app routers at startup — nothing more |
| DB driver, connection pooling, row mapping | SQLx | Djogi wraps SQLx into a typed ORM layer (`Model`, `QuerySet`, `FromRow`, `ConditionBuilder`) but never hides it — raw `sqlx::QueryBuilder` is always an escape hatch |
| ID generation | HeeRanjId | Calls `generate_id()` / `generate_ids(n)` / `generate_ranj_id()` — owns nothing else |
| Async runtime | Tokio | Uses it; does not configure it |
| Model ↔ DB mapping | **Djogi** | Proc macro, field types, QuerySet, relations |
| Migrations | **Djogi** | Differ, generation, snapshot management |
| Shell | **Djogi** | Rhai REPL, model bindings, transaction control |
| CRUD logging | **Djogi** | Audit trail, JSON-aware diffing |
| JSONB schema fields | **Djogi** | `Jsonb<T>` — typed schemas over JSONB columns |
| Admin panel | **Djogi** (opt-in) | Auto-generated from `ModelDescriptor`; Dioxus fullstack; `admin` feature flag |

## Core Dependencies

| Dependency | Role | Notes |
|---|---|---|
| `axum` | HTTP layer, routing, middleware | Core — not abstracted away from developer |
| `sqlx` | Async DB driver + migration runner | Postgres only |
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
        routes.rs           # Axum handlers
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
| [Query API](./docs/spec/queries.md) | QuerySet, conditions, programmatic filters, ConditionBuilder |
| [JSONB Schema Fields](./docs/spec/jsonb.md) | `Jsonb<T>`, unknown field preservation, validation, subfield queries |
| [Relations](./docs/spec/relations.md) | ForeignKey, ManyToMany, explicit through models |
| [Primary Keys](./docs/spec/primary-keys.md) | HeeRanjId: HeerId (64-bit), RanjId (128-bit UUIDv8), generation patterns |
| [Migrations](./docs/spec/migrations.md) | Build-time drift detection, schema snapshots, differ |
| [Logging](./docs/spec/logging.md) | Three-database architecture, CRUD audit trail, event tracing |
| [Configuration & CLI](./docs/spec/configuration.md) | `Djogi.toml`, `cargo djogi`, app registration, Axum integration |
| [Shell](./docs/spec/shell.md) | Rhai REPL, transactions, import/export, seed scripts |
| [Admin Panel](./docs/spec/admin.md) | Dioxus ModelForms, list views, validation, M2M inlines |
| [Research Areas](./docs/spec/research.md) | Open implementation questions by subsystem |
| [Design Decisions](./docs/spec/decisions.md) | Full decision log |

## Background

- [The Agentic Shift](./docs/hypothesis.md) — why Model-first frameworks exist
