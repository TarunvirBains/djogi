> [Back to Index](./index.md)

# Reserved Identifier Namespace

Djogi reserves identifiers beginning with `__djogi_` (two leading underscores + ASCII-case-insensitive `djogi` + underscore) for framework-internal use. Adopters must not emit identifiers in this namespace from `#[model]` definitions, custom column names, window-function aliases, or any other surface that flows into framework-emitted SQL or macro expansions.

The prefix shape is intentional: SQL accepts a leading underscore in identifiers, and Rust keyword shadowing handles double-underscore prefixes cleanly, so `__djogi_*` collides with no Postgres-internal pattern and no idiomatic user code while remaining unambiguous in framework-emitted output.

## Why a reserved namespace

Djogi emits SQL whose internal scaffolding (recursive CTE names, derived-table aliases, synthetic columns the row decoder reads by name) shares a query scope with user-supplied column names and aliases. Without a reserved prefix, every framework feature would either:

1. Pick a one-off random name and pray no adopter happens to use it — fragile across crate versions.
2. Quote-escape every identifier — verbose, breaks `pg_typeof` introspection, and Postgres folds case for unquoted identifiers anyway.
3. Force adopters into a global registry of framework-known column names — defeats the point of a derive-driven framework.

A reserved prefix is the smallest viable contract: framework gets a stable namespace, adopters get a single rule to remember.

## Inventory — what lives in the namespace as of v0.1.0

The table groups identifiers by which compilation surface emits them. Each row links to the file containing the canonical definition / first emission site.

### SQL CTE names

| Identifier | Scope | Defined / emitted in | Purpose |
|---|---|---|---|
| `__djogi_tree` | Recursive CTE | [`djogi/src/query/recursive.rs`](../../djogi/src/query/recursive.rs) | The recursive CTE backing `tree_descendants` / `tree_ancestors` / `full_ancestors`. Carries `(depth, path, <T cols>)`. |
| `__djogi_closure` | Recursive CTE | [`djogi/src/query/closure.rs`](../../djogi/src/query/closure.rs) | The recursive CTE backing `Model::materialize_closure`. Carries `(source_id, ancestor_id, depth, path)`. |

### SQL derived-table / subquery aliases

| Identifier | Scope | Defined / emitted in | Purpose |
|---|---|---|---|
| `__djogi_q` | Outer derived-table alias | [`djogi/src/expr/window_fn.rs`](../../djogi/src/expr/window_fn.rs) | The wrapper alias for `.qualify(...)` lowering — Postgres has no `QUALIFY` clause, so window-fn predicates lift into an outer `SELECT * FROM (<inner>) AS __djogi_q WHERE …`. |

### SQL synthesized columns (aliases inside SELECT)

| Identifier | Scope | Defined / emitted in | Purpose |
|---|---|---|---|
| `__djogi_agg_N` (N ∈ 0..3) | Column alias inside annotate-tuple SELECT | [`djogi/src/query/annotate.rs`](../../djogi/src/query/annotate.rs) | Slot aliases for `QuerySet::annotate(...)` aggregate tuples. The row decoder reads each slot by alias to keep positional decode stable across user `SELECT`-list reorderings. |
| `__djogi_edge_label` | Column alias inside lateral-fan-out subquery | [`djogi/src/query/recursive.rs`](../../djogi/src/query/recursive.rs) | Synthetic text column tagging each lateral alternative with the self-FK edge name. The outer SELECT splices it into the `path` array so callers can distinguish `["mother_id", "father_id"]` from `["father_id", "mother_id"]`. |
| `__djogi_search_seq` | Column alias on the recursive CTE's `SEARCH BREADTH FIRST BY` / `SEARCH DEPTH FIRST BY` clause | [`djogi/src/query/recursive.rs`](../../djogi/src/query/recursive.rs) | The synthetic sequence column Postgres assigns when `with_search_breadth_first_by` / `with_search_depth_first_by` is used. The outer SELECT's auto-prepended `ORDER BY __djogi_search_seq` sorts the result set into BFS / DFS order. Internal — never returned to the caller. |
| `__djogi_parent_id` | Column alias inside prefetch SELECT | [`djogi/src/relation/prefetch.rs`](../../djogi/src/relation/prefetch.rs) | The parent-id column the prefetch decoder reads to stitch eager-loaded relations back to their owners. Aliased on the inner `SELECT` so it cannot collide with a user column literally named `parent_id`. |
| `__djogi_old__<col>` | Column alias inside PG18 OLD/NEW RETURNING projection | [`djogi/src/query/sql.rs`](../../djogi/src/query/sql.rs), [`djogi-macros/src/model/crud.rs`](../../djogi-macros/src/model/crud.rs) | Per-column alias for the pre-update (OLD) row snapshot returned by `RETURNING WITH (OLD AS __djogi_old, ...)`. Used by `Model::update_returning_pair` and `UpdateStmt::execute_returning_pairs`. Decoded via `FromJoinedPgRow` with prefix `"__djogi_old__"`. (PG18 only.) |
| `__djogi_new__<col>` | Column alias inside PG18 OLD/NEW RETURNING projection | [`djogi/src/query/sql.rs`](../../djogi/src/query/sql.rs), [`djogi-macros/src/model/crud.rs`](../../djogi-macros/src/model/crud.rs) | Per-column alias for the post-update (NEW) row snapshot returned by `RETURNING WITH (..., NEW AS __djogi_new)`. Used by `Model::update_returning_pair` and `UpdateStmt::execute_returning_pairs`. Decoded via `FromJoinedPgRow` with prefix `"__djogi_new__"`. (PG18 only.) |

### Macro-emitted identifiers (in user crate scope)

These are emitted by `#[derive(...)]` and attribute-macros into the user's crate. Most live in private scopes (function bodies, anonymous modules) so collision with user code is practically bounded — but the prefix is still reserved.

| Identifier root | Emitted by | Purpose |
|---|---|---|
| `__djogi_test_inner_<N>` | `#[djogi_test]` | Inner test-fn name — wraps the user's body so the outer fn can do per-test database provisioning. |
| `__djogi_apps_invocation_sentinel` | `djogi::apps!` | Invocation sentinel — proves `djogi::apps!` was called somewhere in the binary. |
| `__djogi_auth`, `__djogi_auth_present` | Auth machinery | Authentication context markers in macro expansions. |
| `__djogi_peer` | Visage `expose(scope -> Peer)` | Peer-visage helper struct emitted to thread `TryFrom<&Target>` through visage projections. |
| `__djogi_through_visage_exists` | `many_to_many!` `expose` | Static check that the through-model declares the requested visage scope. |
| `__djogi_rationale_outbox_*` | `#[outbox]`-decorated rationales | Per-rationale outbox helper symbols. |
| `__djogi_cond`, `__djogi_rel`, `__djogi_inner`, `__djogi_exists`, `__djogi_path`, `__djogi_tid`, `__djogi_tid_str` | Various proc-macro emissions | Internal scratch identifiers in macro-emitted code. |

## Validation surfaces

The framework rejects user-supplied identifiers in this namespace at the surfaces below. Each surface validates at the point user input crosses into framework-emitted SQL or expansion. The rule is centralized in the [`crate::ident`](../../djogi/src/ident.rs) module: runtime callers route through `check_user_supplied_ident` / `assert_user_supplied_ident`, which return / panic with `IdentError::ReservedDjogiPrefix` for any identifier in the `__djogi_*` namespace, matched ASCII-case-insensitively because Postgres folds unquoted identifiers to lowercase. The macro-time validator in [`djogi-macros/src/ident.rs`](../../djogi-macros/src/ident.rs) carries the same rule as a sibling `RESERVED_DJOGI_PREFIX` constant pinned in unit tests on both sides.

| Surface | Enforcement | File |
|---|---|---|
| Window-function `.alias(&str)` (every `Window*` builder, including `RowNumber`, `Rank`, `DenseRank`, `PercentRankWindow`, `CumeDistWindow`, `NtileWindow`, `FirstValueWindow`, `LastValueWindow`, `LeadWindow`, `LagWindow`, `NthValueWindow`) | `assert_user_supplied_ident(alias, "window_alias")` | [`djogi/src/expr/window_fn.rs`](../../djogi/src/expr/window_fn.rs) |
| `ClosureModel` column accessors (`source_column`, `ancestor_column`, `depth_column`, `path_count_column`, plus `table()`) | `check_user_supplied_ident(col, true)` → `DjogiError::Validation` on reservation hit | [`djogi/src/query/closure.rs`](../../djogi/src/query/closure.rs) |
| FTS `dictionary` / `source` column names from `#[model(fts = ...)]` | `check_user_supplied_ident(name, false)` with a reservation-specific diagnostic | [`djogi/src/fts.rs`](../../djogi/src/fts.rs) |
| Outbox table name (worker-side validation in `claim_pending` / `mark_published` / `mark_failed` / `recover_stale`) | `check_user_supplied_ident(name, false)` → `DjogiError::Db` on reservation hit | [`djogi/src/outbox/worker.rs`](../../djogi/src/outbox/worker.rs) |
| Hidden testing outbox-table helpers (`outbox_rows_for_test`, `clear_outbox_for_test`) | `check_user_supplied_ident(table, false)` before raw SQL embedding | [`djogi/src/testing.rs`](../../djogi/src/testing.rs) |
| Runtime enum-type names (`DjogiContext::ensure_enum_type`) | `check_user_supplied_ident(value, false)` → `DjogiError::Db` on reservation hit | [`djogi/src/context.rs`](../../djogi/src/context.rs) |
| `#[model]` field column names and `#[model(table = "...")]` (macro-time) | `check_ident(...)` rejects the `__djogi_` prefix ASCII-case-insensitively with a `syn::Error` carrying a rename hint | [`djogi-macros/src/ident.rs`](../../djogi-macros/src/ident.rs) |
| Reverse-relation / M2M macro names (`reverse_one_to_*` relation `name` / `via`, `many_to_many!` `relation`, `this_fk`, `that_fk`) | `const_assert_user_supplied_ident(...)` in the sealed registry constructor / guard const, rejecting the prefix at const-eval before inventory submission | [`djogi/src/relation/registry.rs`](../../djogi/src/relation/registry.rs), [`djogi-macros/src/many_to_many.rs`](../../djogi-macros/src/many_to_many.rs), [`djogi-macros/src/reverse_relation.rs`](../../djogi-macros/src/reverse_relation.rs) |
| Grouped-fetch SELECT-list collision detector | `assert_no_alias_collision(sql)` returns `DjogiError::AliasCollision { alias }` when the parsed SELECT list contains two columns with the same alias. The check is generic (any duplicate alias is rejected), but it indirectly enforces the `__djogi_*` rule for `__djogi_agg_N`: when the framework emits `<agg> AS __djogi_agg_0`, a user SELECT alias of `__djogi_agg_0` produces a duplicate that the detector catches. (Phase 6.5 — aggregate alias discipline.) | [`djogi/src/query/sql.rs`](../../djogi/src/query/sql.rs) |

Surfaces that derive identifiers from already-validated inputs (e.g. the `<table>_outbox` companion table and the `djogi_<table>` notify channel in [`djogi/src/outbox/mod.rs`](../../djogi/src/outbox/mod.rs) and [`djogi/src/notify.rs`](../../djogi/src/notify.rs)) remain on the plainer `check_plain_ident`, since the reservation rule is transitively enforced upstream by the macro-time gate on the source table name.

## Coverage status (v0.1.0)

GH issues [#69](https://github.com/TarunvirBains/djogi/issues/69) (lift the prefix check out of `WindowExpr::alias`) and [#82](https://github.com/TarunvirBains/djogi/issues/82) (uniform rejection across user-facing entry points) are closed by the central `IdentError::ReservedDjogiPrefix` variant and the `check_user_supplied_ident` / `assert_user_supplied_ident` helpers. The runtime helpers continue to accept the `__djogi_*` prefix on the general-purpose `check_plain_ident` / `assert_plain_ident` entry points, so framework-internal emissions (recursive CTEs, derived-table aliases, slot aliases) keep working without per-call-site allowlists.

## Adopter contract

In short: never emit `__djogi_*` from user code. The compile-fail / runtime-error fallout is small, but future framework changes can repurpose any name in the namespace without considering it a breaking change.

If you have a legitimate reason to use `__djogi_*` in your own code (e.g. you maintain a derivative crate that wants its own private prefix), pick a distinct prefix — `__myprefix_*` is fine, `__djogi_*` is not.

## Stability contract

- The set of `__djogi_*` identifiers is internal — names can be added, removed, or renamed without semver implications.
- The reservation rule itself (the prefix shape) is part of djogi's public contract and won't change.
