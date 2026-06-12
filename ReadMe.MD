# Djogi

*A Postgres-native distributed data engine for Rust, driven by model declarations. Define your models as structs; Djogi derives the ORM, migrations, audit trail, and more from a single model definition.*

---

Most Rust data libraries solve one slice — a query builder, migrations, or an audit log. Djogi treats the data tier as a single, model-first surface: typed queries, schema evolution, relations, change tracking, row-level security, transport projections via visages, in-memory cache, cross-runtime predicate algebra, and spatial predicates all derive from your model declarations. You bring your own HTTP/application stack; Djogi owns the data layer end-to-end. Raw SQL is a deliberate escape hatch when you need edge-case control.

## Why Postgres Only

Djogi targets PostgreSQL exclusively — permanently, not as a temporary limitation. Djogi depends on JSONB path operators for typed schema fields, advisory locks for migration safety, `RETURNING` for atomic round-trips, row-level locking, rich indexing, and transactional DDL. Abstracting over multiple databases would mean abandoning these capabilities or reimplementing them poorly. Every query Djogi generates targets Postgres directly; there is no lowest-common-denominator SQL layer.

## Design Principles

- **Define once, derive everything** — `#[derive(Model)]` produces typed CRUD, migration diffs, audit hooks, and transport projections from a single struct declaration.
- **Explicit over implicit** — No hidden middleware chains or convention-as-code. Behavior is visible in typed APIs and generated descriptors.
- **Postgres-first, not portable abstraction** — Djogi maps core features directly to native Postgres capabilities instead of lowest-common-denominator SQL.
- **Your app stack, your handlers** — Djogi integrates with Axum, Warp, Actix, Rocket, and others. Routing, middleware, and rendering remain yours.
- **Performance and safety defaults** — Type-first APIs cover common production workloads; raw SQL remains an explicit escape hatch for rare edge cases.

## What You Get

| Capability | Guide |
|---|---|
| Models & schema derivation via `#[derive(Model)]` | [Models guide](docs/guide/models.md) |
| Lazy typed queries with relations and eager loading | [Queries guide](docs/guide/queries.md), [Relations guide](docs/guide/relations.md) |
| Build-time migrations with advisory-locked runner | [Migrations guide](docs/guide/migrations.md) |
| Typed JSONB columns preserving unknown fields | [JSONB guide](docs/guide/jsonb.md) |
| Spatial types and geographic predicates | [Spatial guide](docs/guide/spatial.md) |
| Auth context, password hashing, multi-tenancy & RLS | [Auth guide](docs/guide/auth.md), [Tenancy guide](docs/guide/tenancy.md) |
| Audience-specific transport projections via visages, in-memory cache, and cross-runtime predicate algebra | [Visages guide](docs/guide/visages.md) |
| Transactions, outbox pattern, expressions & aggregates | [Transactions](docs/guide/transactions.md), [Outbox](docs/guide/outbox.md), [Expressions](docs/guide/expressions.md) guides |

**Planned (not yet shipped):** Rhai interactive shell for model inspection and query building; Maahi admin console — a Dioxus operations UI derived from your model descriptors.

## Status

**Current release:** `v0.1.0-alpha.9` (public alpha — API may change before v0.1.0 stable)

## Project Layout

```
my-app/
  src/
    main.rs
    apps/
      vehicles/
        mod.rs              # djogi::apps! { ... }
        models.rs           # #[derive(Model)] structs + relations
        routes.rs           # handlers for your application stack
  build.rs                  # drift detection + migration generation
  Djogi.toml                # configuration
  migrations/               # managed by pipeline (git submodule)

djogi/                      # core data library crate
djogi-macros/               # proc macro crate
djogi-cli/                  # standalone `djogi` binary
```

## Local Development

The `.scripts/` directory is a git submodule pointing to [`TarunvirBains/scripts`](https://github.com/TarunvirBains/scripts). Clone with submodules:

```bash
git clone --recurse-submodules https://github.com/TarunvirBains/djogi.git
# or, if you already cloned:
git submodule update --init --recursive
```

Reproduce CI locally:

```bash
./.scripts/ci-local.sh
```

## Specification & Guides

The full specification lives in [`docs/spec/`](docs/spec/index.md). Key docs:

| Doc | Covers |
|---|---|
| [Models](docs/spec/models.md) | `#[derive(Model)]`, field types, annotations |
| [Query API](docs/spec/queries.md) | QuerySet, conditions, filters |
| [Relations](docs/spec/relations.md) | ForeignKey, ManyToMany, through models |
| [JSONB Schema Fields](docs/spec/jsonb.md) | `Jsonb<T>`, unknown field preservation, subfield queries |
| [Primary Keys](docs/spec/primary-keys.md) | HeerId, RanjId, custom PK integration |
| [Migrations](docs/spec/migrations.md) | Drift detection, schema snapshots, differ |
| [Logging](docs/spec/logging.md) | CRUD audit trail, event tracing |
| [Configuration & CLI](docs/spec/configuration.md) | `Djogi.toml`, CLI commands, app registration |
| [Architecture Principles](docs/spec/architecture-principles.md) | Framework boundaries and design decisions |
| [Scope & Boundaries](docs/spec/scope.md) | What belongs in Djogi vs an app crate |
