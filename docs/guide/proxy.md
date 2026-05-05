# Proxy Models

> **Phase**: 8 Cluster 8β — Proxy + Computed Properties
> **Status**: v0.1.0
> **Spec anchor**: `docs/spec/implementation-plan.md` §628

A **proxy model** is a Rust struct that shares its database table with another
("parent") model but exposes its own CRUD surface, default ordering, default
filter, and per-type lifecycle hooks. Proxies are the right shape when you
want to model a behavioural slice of a base table — "active vehicles", "soft-
deleted users", "archived orders" — without copying every field declaration
or maintaining a separate table.

This guide covers:

- When to reach for a proxy versus a regular model with `.filter(...)`.
- The `#[model(proxy_for, default_filter, default_order)]` attribute set.
- How proxy querysets compose with adopter-side `.filter(...)` /
  `.order_by(...)` calls.
- The migration-differ schema-passthrough behaviour (proxies do not emit
  DDL).
- Constraints adopters must respect.

## When to use a proxy model

Reach for a proxy when **all four** apply:

1. The behavioural slice has a stable name worth modelling at the type level
   (`ActiveVehicle`, not `Vehicle::objects().filter(|f| f.active.eq(true))`
   sprinkled across the codebase).
2. The slice always applies — every query against `ActiveVehicle` should
   include the `active = TRUE` filter; forgetting the filter would be a
   correctness bug, not a feature.
3. The slice is invariant in scope — the filter does not depend on the
   adopter, request context, or runtime state. Use a regular `.filter(...)`
   call when the predicate carries an `Arc<AuthContext>` or similar.
4. You want per-type lifecycle hooks (`before_create`, `after_save`, etc.)
   that fire for the slice but not for the parent — for example, `ActiveVehicle`
   may carry an audit-log hook that `Vehicle` itself does not.

If any of those conditions fail, prefer a regular `.filter(...)` call at the
query site over a proxy declaration.

## Declaring a proxy

The parent model is a regular `#[model(...)]` declaration:

```rust
use djogi::prelude::*;

#[model(table = "vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    pub model: String,
    pub price: i64,
    pub active: bool,
    pub archived: bool,
}
```

The proxy declares the **same** table name + a `proxy_for = ParentType`
attribute. Field declarations mirror the parent (proxies and parents share
storage; the field shape must match):

```rust
#[model(
    table = "vehicles",
    proxy_for = Vehicle,
    default_filter = |f| f.active.eq(true),
    default_order = [(price, Desc)],
)]
#[derive(Debug, Clone)]
pub struct ActiveVehicle {
    pub make: String,
    pub model: String,
    pub price: i64,
    pub active: bool,
    pub archived: bool,
}
```

Two attributes drive proxy behaviour:

- **`default_filter = |f| ...`** — a closure returning a filter
  predicate over the model's field accessors. AND-composed into every
  `QuerySet<ProxyModel>` on construction.
- **`default_order = [(field, Asc|Desc), ...]`** — ordering applied to every
  freshly constructed queryset. Adopter `.order_by(...)` calls **append**,
  not replace (matching the existing queryset convention; one rule for every
  queryset shape).

Both are optional — declaring `proxy_for = Parent` without either is valid
(useful when you only want per-type hooks).

## What's allowed in `default_filter`

The closure body parses through a closed grammar at macro-expand time. The
v0.1.0 surface accepts:

- Field accessors `f.<column>` (single-segment ident; the binding identifier
  matches the closure's parameter name).
- Comparison predicates: `eq`, `neq`, `gt`, `gte`, `lt`, `lte`.
- Null predicates: `is_null`, `is_not_null`.
- Range predicates: `between(lo, hi)`.
- Boolean combinators: `.and_with(...)`, `.or_with(...)`.
- Inline literals: `bool`, integer, float, string, the keyword `null`.

Anything outside this grammar — runtime variables, function calls,
unrecognised methods — surfaces a span-precise compile error pointing at the
offending node and instructing you to implement
`Model::default_filter_condition` by hand for the runtime-bound RHS case.

The closed grammar is intentionally narrow (per the project's
`feedback_no_regex_in_djogi.md` no-regex rule + the lens resolution that
runtime-bound closures depend on cluster 8γ's Q-Algebra). Adopters who hit
the rejection path either:

- Use an inline literal where possible.
- Implement `Model::default_filter_condition` by hand (returns
  `Option<Condition>` constructed via the typed `Condition::and()` /
  `Condition::or()` builders).

## What `default_order` accepts

A non-empty list of `(field_ident, Asc|Desc)` tuples. The list parses as a
literal Rust array expression:

```rust
default_order = [(price, Desc), (created_at, Asc)],
```

Empty arrays (`default_order = []`) are rejected at parse time — silently
emitting no override would surface as adopter confusion ("why did my
`default_order` disappear?"). Either provide at least one entry or omit the
attribute entirely.

`Asc` / `Desc` are bare identifiers (no string literals); `NullsOrder` is
fixed at the Postgres default for v0.1.0. Override per-call via the regular
`OrderExpr::nulls_first()` / `nulls_last()` modifiers when needed.

## Composition with adopter `.filter(...)` / `.order_by(...)`

The proxy default filter is the **prefix** that no adopter call can drop:

```rust
ActiveVehicle::objects()
    .filter(|f| f.price.gte(50000))
    .fetch_all(&mut ctx).await?;

// Emits SQL roughly:
//   SELECT ... FROM vehicles WHERE ((active = TRUE) AND price >= $1)
//   ORDER BY price DESC
```

The proxy default ordering is the **prefix** to which adopter `.order_by(...)`
calls append:

```rust
ActiveVehicle::objects()
    .order_by(|f| f.id.asc())
    .fetch_all(&mut ctx).await?;

// Emits SQL roughly:
//   SELECT ... FROM vehicles WHERE (active = TRUE)
//   ORDER BY price DESC, id ASC
```

The append rule keeps one consistent ordering rule across every queryset
shape (proxy or non-proxy) — adopters never need to remember whether their
`.order_by(...)` replaces or composes.

## Bulk operations

`ActiveVehicle::objects().delete()` AND-composes the default filter into the
DELETE's WHERE clause:

```sql
DELETE FROM vehicles WHERE (active = TRUE)
```

Per [Decision D5](../spec/decisions.md), there is **no runtime warning** on
proxy bulk-delete — the proxy filter scopes the operation correctly by
construction. Adopters who want to bypass the proxy filter and operate on the
parent table use `Vehicle::objects().delete()` directly.

The same scoping applies to `bulk_update`, `bulk_upsert`, and any other
operation that translates to a WHERE-clause-bearing SQL statement.

## Migration-differ behaviour (schema-passthrough)

Proxy descriptors are **skipped** from migration-differ DDL emission. The
parent model owns the table; the proxy contributes no separate `CREATE TABLE`,
no separate indexes, no separate FK constraints. Two proxies of the same
parent coexist without surfacing duplicate-table-in-bucket collisions.

### Constraint: tables must match

The proxy's `table = "..."` declaration must match the parent's `table` value.
The macro accepts the declaration verbatim and the cross-type "tables match"
invariant is enforced at descriptor-lookup time — declaring a different table
on the proxy will surface as a runtime error when the differ tries to
register both descriptors.

The macro could enforce this at parse time via cross-type lookup, but proc
macros cannot reach across type-definition sites at expansion. The runtime
check is the v0.1.0 backstop; future phases may add a build-time descriptor
audit pass.

### Constraint: field shapes must match

Proxies share storage with their parent. If the parent declares `pub price:
i64` and the proxy declares `pub price: f64`, the resulting `FromPgRow`
decoding will fail. Keep the field set + types identical between proxy and
parent.

## Composition with `#[derive(SoftDeletable)]` / `#[derive(Auditable)]`

When a proxy AND its parent both use `#[model(soft_deletable)]`, the proxy's
`default_filter` AND-composes with the parent's soft-delete predicate
(`deleted_at IS NULL`). The proxy inherits the parent's soft-delete and
applies its own filter on top — no silent override.

Same for `#[model(auditable)]` populator behaviour: the parent's auditable
hook fires; the proxy's per-type lifecycle hooks (if any) fire alongside.

## Constraints summary

- The proxy's `table = "..."` MUST match the parent's.
- The proxy's field set + types SHOULD match the parent's (storage is
  shared; mismatched types fail decode).
- `default_filter` accepts only inline-literal RHS (bool / integer / float /
  string / `null`); runtime-bound RHS requires hand-implementing
  `Model::default_filter_condition`.
- `default_order` requires a non-empty array if present.
- Generic proxies (`#[model(proxy_for = Vehicle<T>)]`) are not yet
  supported — concrete-type proxies only in v0.1.0.

## Related documents

- [Models](models.md) — base `#[model(...)]` attribute reference.
- [Queries](queries.md) — `QuerySet<T>` API, `.filter(...)`, `.order_by(...)`.
- [Composition](composition.md) — `#[derive(Auditable)]` / `#[derive(SoftDeletable)]`.
