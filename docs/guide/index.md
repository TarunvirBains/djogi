> [Back to README](../../ReadMe.MD)

# Djogi Guides

Documents describing the shipped framework surface (Phases 1 through 5).
For planned features that don't ship yet, see
[the roadmap](../roadmap/index.md).

| Guide | Covers |
|---|---|
| [Getting Started](./getting-started.md) | Installation, first model, first CRUD, first test |
| [Models](./models.md) | `#[model(...)]` attributes, `#[field(...)]` attributes, Phase 1 field types |
| [Queries](./queries.md) | `QuerySet<T>`, filter closures, programmatic filters, bulk update/delete |
| [Relations](./relations.md) | `ForeignKey<T>`, `OneToOneField<T>`, prefetch, `select_related`, reverse accessors, explicit-through M2M |
| [Transactions](./transactions.md) | `DjogiContext`, `atomic()`, savepoint nesting, `on_commit`, row locks, `retry_on_conflict` |
| [Expressions](./expressions.md) | `Expr<T>`: arithmetic, field-vs-field, CASE/WHEN, subqueries, typed `OuterRef`, aggregates, annotations |
| [Outbox](./outbox.md) | `#[model(events)]`, `#[field(outbox = "ignore")]`, rollback semantics, publisher patterns |
| [Visages](./visages.md) | `#[field(expose(...))]`, `{Model}Public/SelfView/Admin/Export`, `From`/`TryFrom`, `VisageError` |
| [Tracked Fields](./tracked-fields.md) | `Tracked<T>` dirty-tracking wrapper, selective column writes, `mark_clean` |
| [Optimistic Locking](./optimistic-locking.md) | `#[field(version)]`, version predicate in `save()`, `LockConflict`, retry patterns |
| [Enums](./enums.md) | `#[derive(DjogiEnum)]`, Postgres codec, `rename_all`, per-variant overrides |
| [JSONB Fields](./jsonb.md) | `Jsonb<T>`, unknown-field preservation, flat path querying, `#[derive(JsonbSchema)]` typed paths |
| [Array Fields](./arrays.md) | `Vec<V>` columns, `contains` / `contained_by` / `overlap` / `len`, GIN index intent |
| [Tenancy](./tenancy.md) | `#[model(tenant_key)]`, RLS policy emission, `set_tenant`, `_insecurely()` bypass |
| [Agent Guide](./agent-guide.md) | For AI coding agents — reading Djogi code, golden path, common mistakes |
