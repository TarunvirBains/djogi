> [Back to Index](./index.md)

# Reserved Identifier Namespace

Djogi reserves identifiers beginning with `__djogi_` (two leading underscores + lowercase `djogi` + underscore) for framework-internal use. Adopters must not emit identifiers in this namespace from `#[model]` definitions, custom column names, window-function aliases, or any other surface that flows into framework-emitted SQL or macro expansions.

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
| `__djogi_parent_id` | Column alias inside prefetch SELECT | [`djogi/src/relation/prefetch.rs`](../../djogi/src/relation/prefetch.rs) | The parent-id column the prefetch decoder reads to stitch eager-loaded relations back to their owners. Aliased on the inner `SELECT` so it cannot collide with a user column literally named `parent_id`. |

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

The framework rejects user-supplied identifiers in this namespace at the surfaces below. Each surface validates at the point user input crosses into framework-emitted SQL or expansion.

| Surface | Enforcement | File |
|---|---|---|
| `WindowExpr::alias(&str)` | `assert!(!alias.starts_with("__djogi_"))` | [`djogi/src/expr/window_fn.rs:326`](../../djogi/src/expr/window_fn.rs) |
| `QuerySet::annotate(...)` aggregate alias collisions | `DjogiError::AnnotationAliasCollision` rejects user SELECT aliases starting with `__djogi_agg_` (Phase 6.5 decision) | [`djogi/src/query/annotate.rs`](../../djogi/src/query/annotate.rs) |

## Coverage gap (v0.1.0)

The central identifier validator [`crate::ident::check_plain_ident`](../../djogi/src/ident.rs) does **not** check the `__djogi_` prefix. The two surfaces above are the only places that enforce the rule. Other call sites that route user-supplied identifiers through `check_plain_ident` (e.g. `ClosureModel::source_column()` / `ancestor_column()` / `depth_column()` / `path_count_column()` in `closure.rs`, and FTS dictionary / source-column names in `fts.rs`) currently let `__djogi_*` through.

In practice this is theoretical — those surfaces emit identifiers in scopes that don't share a name-resolution context with the framework's recursive CTE columns or derived-table aliases. But the rule should be uniform; tracked as **GH issue #82** for follow-up.

## Adopter contract

In short: never emit `__djogi_*` from user code. The compile-fail / runtime-error fallout is small, but future framework changes can repurpose any name in the namespace without considering it a breaking change.

If you have a legitimate reason to use `__djogi_*` in your own code (e.g. you maintain a derivative crate that wants its own private prefix), pick a distinct prefix — `__myprefix_*` is fine, `__djogi_*` is not.

## Stability contract

- The set of `__djogi_*` identifiers is internal — names can be added, removed, or renamed without semver implications.
- The reservation rule itself (the prefix shape) is part of djogi's public contract and won't change.
