# Djogi

*A Postgres-native distributed data engine for Rust, built around model declarations. Define your models as structs, and Djogi derives the ORM, migrations, audit trail, and more from a single model definition.*

---

Most Rust data libraries solve one slice — a query builder, migrations, or an audit log. Djogi treats the data tier as a single, model-first surface: typed queries, typed CTEs, schema evolution, relations, change tracking, row-level security, transport projections via visages, in-memory cache, cross-runtime predicate algebra, and spatial predicates all derive from your model declarations. You bring your own HTTP/application stack; Djogi owns the data layer end-to-end.

## Why Postgres Only

Djogi targets PostgreSQL exclusively — permanently, not as a temporary limitation. Djogi depends on JSONB path operators for typed schema fields, advisory locks for migration safety, `RETURNING` for atomic round-trips, row-level locking, rich indexing, and transactional DDL. Abstracting over multiple databases would mean abandoning these capabilities or reimplementing them poorly. Every query Djogi generates targets Postgres directly; there is no lowest-common-denominator SQL layer.

## Design Principles

- **Define once, derive everything** — `#[derive(Model)]` produces typed CRUD, migration diffs, audit hooks, and transport projections from a single struct declaration.
- **Explicit over implicit** — No hidden middleware chains or convention-as-code. Behavior is visible in typed APIs and generated descriptors.
- **Postgres-first, not portable abstraction** — Djogi maps core features directly to native Postgres capabilities instead of lowest-common-denominator SQL.
- **Your app stack, your handlers** — Djogi integrates with Axum, Warp, Actix, Rocket, and others. Routing, middleware, and rendering remain yours.
- **Performance and safety defaults** — Type-first APIs cover common production workloads, with explicit escape hatches available for exceptional requirements.

## What You Get

| Capability | Guide |
|---|---|
| Models & schema derivation via `#[derive(Model)]` | [Models guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/models.md) |
| Lazy typed queries with relations and eager loading | [Queries guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/queries.md), [Relations guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/relations.md) |
| Typed `WITH` / `WITH RECURSIVE` queries with cycle filtering | [Queries guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/queries.md) |
| Build-time migrations with advisory-locked runner | [Migrations guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/migrations.md) |
| Typed JSONB columns preserving unknown fields | [JSONB guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/jsonb.md) |
| Spatial types and geographic predicates | [Spatial guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/spatial.md) |
| Auth context, password hashing, multi-tenancy & RLS | [Auth guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/auth.md), [Tenancy guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/tenancy.md) |
| Audience-specific transport projections via visages, in-memory cache, and cross-runtime predicate algebra | [Visages guide](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/visages.md) |
| Transactions, outbox pattern, expressions & aggregates | [Transactions](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/transactions.md), [Outbox](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/outbox.md), [Expressions](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/expressions.md) guides |

## Typed CTE Queries

Djogi now exposes typed Common Table Expressions directly from `QuerySet`: plain `WITH` terms through `.with(...)`, recursive `WITH RECURSIVE` terms through `.with_recursive(...)`, consumer selection through `.from_cte(...)`, and Postgres cycle filtering through `.cycle(...)` plus `.exclude_cycle_rows()`. This covers the common “build a reusable subquery, then read from it” and “walk a hierarchy without dropping to raw SQL” cases inside the typed query surface.

```rust
use djogi::prelude::*;

let active_posts = Post::objects().filter(|f| f.published().eq(true));

let rows = Post::objects()
.with("active_posts", active_posts)?
.from_cte("active_posts")?
.fetch_all(&mut ctx)
.await?;

let ancestors = Category::objects()
.with_recursive(
  "ancestors",
  Category::objects().filter(|f| f.id().eq(target_id)),
  RecursiveArm::<Category>::referencing("ancestors")
  .join_on("id", "parent_id")?,
 )?
.from_cte("ancestors")?
.fetch_all(&mut ctx)
.await?;
```

When a query shape still falls outside the typed surface, the raw SQL escape hatches remain available. The point is that ordinary non-recursive and recursive CTE work no longer needs them.

**Planned (not yet shipped):** Rhai interactive shell for model inspection and query building; Maahi admin console — a Dioxus operations UI derived from your model descriptors.

## Status

**Current release:** `v0.1.0-alpha.11` (public alpha — API may change before v0.1.0 stable)

## Project Layout

```
my-app/
 src/
 main.rs
 apps/
  vehicles/
  mod.rs    # djogi::apps! {... }
  models.rs   # #[derive(Model)] structs + relations
  routes.rs   # handlers for your application stack
 build.rs     # drift detection + migration generation
 Djogi.toml    # configuration
 migrations/    # managed by pipeline (git submodule)

djogi/      # core data library crate
djogi-macros/    # proc macro crate
djogi-cli/     # standalone `djogi` binary
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

The full specification lives in [`docs/spec/`](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/index.md). Key docs:

| Doc | Covers |
|---|---|
| [Models](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/models.md) | `#[derive(Model)]`, field types, annotations |
| [Query API](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/queries.md) | QuerySet, conditions, filters |
| [Relations](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/relations.md) | ForeignKey, ManyToMany, through models |
| [JSONB Schema Fields](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/jsonb.md) | `Jsonb<T>`, unknown field preservation, subfield queries |
| [Primary Keys](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/primary-keys.md) | HeerId, RanjId, custom PK integration |
| [Migrations](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/migrations.md) | Drift detection, schema snapshots, differ |
| [Logging](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/logging.md) | CRUD audit trail, event tracing |
| [Configuration & CLI](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/configuration.md) | `Djogi.toml`, CLI commands, app registration |
| [Architecture Principles](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/architecture-principles.md) | Framework boundaries and design decisions |
| [Raw SQL Escape Hatches](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md) | Bypass points for hand-rolled SQL and direct driver access |
| [Scope & Boundaries](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/scope.md) | What belongs in Djogi vs an app crate |

## Deep Links

- [Query aggregation](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/query-aggregation.md) — `rollup`, `cube`, `group_by_sets`, and typed aggregate forms.
- [Enums](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/enums.md) — Postgres enum modeling via `DjogiEnum`.
- [Arrays](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/arrays.md) — Array field operators and semantics.
- [Tracked fields](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/tracked-fields.md) — Dirty tracking and selective write behavior.
- [Optimistic locking](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/optimistic-locking.md) — Version-based conflict control.
- [Apps](https://github.com/TarunvirBains/djogi/blob/main/docs/guide/apps.md) — Multi-app registration and shared-cluster workflows.
- [Shell](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/shell.md) — Rhai shell trajectory and operational scripting context.
- [Maahi](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/maahi/index.md) — Admin console and RBAC direction.
- [Design decisions](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/decisions.md) — Long-form architecture rationale.
- [The hypothesis](https://github.com/TarunvirBains/djogi/blob/main/docs/hypothesis.md) — Why model-first data tooling.
