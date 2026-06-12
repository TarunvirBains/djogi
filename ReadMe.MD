# Djogi

*A Postgres-native data layer for Rust. Define your schema as structs; Djogi derives the ORM, migrations, audit trail, and more.*

---

Most Rust data libraries solve one slice — a query builder, or migrations, or an audit log. Djogi treats the data tier as a single surface: typed queries, schema evolution, relations, change tracking, row-level security, transport projections, and spatial predicates all integrate from one model definition. You bring your own web framework; Djogi owns the data tier end-to-end.

## Why Postgres Only

Djogi targets PostgreSQL exclusively — permanently, not as a temporary limitation. The framework depends on JSONB with path operators for typed schema fields, advisory locks for migration safety, RETURNING clauses for atomic round-trips, row-level locking, rich indexing, and transactional DDL. Abstracting over multiple databases would mean abandoning these capabilities or reimplementing them poorly. Every query Djogi generates targets Postgres directly; there is no lowest-common-denominator SQL layer.

## Design Principles

- **Define once, derive everything** — `#[derive(Model)]` produces typed CRUD, migration diffs, audit hooks, and transport projections from a single struct declaration.
- **Explicit over implicit** — No hidden middleware chains or convention-as-code. What the framework does is visible in your types and callable APIs.
- **Postgres-first, not Postgres-compatible** — The framework surfaces Postgres capabilities directly rather than hiding them behind a portable abstraction.
- **Your web framework, your view layer** — Djogi integrates with Axum, Warp, Actix, Rocket, or any Rust web framework. Routing, middleware, and rendering are your responsibility.

## What You Get

| Capability | Guide |
|---|---|
| Models & schema derivation via `#[derive(Model)]` | [Models guide](docs/guide/models.md) |
| Lazy typed queries with relations and eager loading | [Queries guide](docs/guide/queries.md), [Relations guide](docs/guide/relations.md) |
| Build-time migrations with advisory-locked runner | [Migrations guide](docs/guide/migrations.md) |
| Typed JSONB columns preserving unknown fields | [JSONB guide](docs/guide/jsonb.md) |
| Spatial types and geographic predicates | [Spatial guide](docs/guide/spatial.md) |
| Auth context, password hashing, multi-tenancy & RLS | [Auth guide](docs/guide/auth.md), [Tenancy guide](docs/guide/tenancy.md) |
| Audience-specific transport projections (visages) | [Visages guide](docs/guide/visages.md) |
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
        routes.rs           # web-framework handlers (any Rust web framework)
  build.rs                  # drift detection + migration generation
  Djogi.toml                # configuration
  migrations/               # managed by pipeline (git submodule)

djogi/                      # framework library crate
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

The full framework specification lives in [`docs/spec/`](docs/spec/index.md). Key docs:

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
