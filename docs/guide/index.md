> [Back to README](../../ReadMe.MD)

# Djogi Guides

Documents describing the shipped framework surface (Phases 1 through 8.5).
For design history and items still on the horizon, see
[the roadmap](../roadmap/index.md).

| Guide | Covers |
|---|---|
| [Getting Started](./getting-started.md) | Installation, first model, first CRUD, first test |
| [Connection Pool](./pool.md) | `DjogiPool` builder, `post_connect` hook, `raw_with_client` raw-driver bypass (via `RawPoolAccessExt`), env > TOML > default sizing |
| [Models](./models.md) | `#[model(...)]` attributes, `#[field(...)]` attributes, Phase 1 field types |
| [Queries](./queries.md) | `QuerySet<T>`, filter closures, programmatic filters, bulk update/delete, set operations (`union`/`intersect`/`except`), INSERT SELECT bulk copy, recursive/tree queries, pair-tuple closure self-joins, `.cache(&pool)?` |
| [Query Aggregation](./query-aggregation.md) | `group_by` / `rollup` / `cube` / `group_by_sets`, `annotate`, `having`, window frames, DISTINCT aggregates, spatial grouping |
| [Relations](./relations.md) | `ForeignKey<T>`, `OneToOneField<T>`, prefetch, `select_related`, reverse accessors, explicit-through M2M |
| [Transactions](./transactions.md) | `DjogiContext`, `atomic()`, savepoint nesting, `on_commit`, row locks, `retry_on_conflict`, production retry backoff |
| [Expressions](./expressions.md) | `Expr<T>`: arithmetic, field-vs-field, CASE/WHEN, subqueries, typed `OuterRef`, aggregates, annotations |
| [Outbox](./outbox.md) | `#[model(events)]`, `#[field(outbox = "ignore")]`, rollback semantics, publisher patterns |
| [Visages](./visages.md) | `#[field(expose(...))]`, `{Model}Public/SelfView/Admin/Export`, `From`/`TryFrom`, `VisageError` |
| [Derived Projections](./derived-projections.md) | `#[derived(...)]`, SQL/Rust parity, nullable derived values, fallible projection conversion |
| [Tracked Fields](./tracked-fields.md) | `Tracked<T>` dirty-tracking wrapper, selective column writes, `mark_clean` |
| [Optimistic Locking](./optimistic-locking.md) | `#[field(version)]`, version predicate in `save()`, `LockConflict`, retry patterns |
| [Enums](./enums.md) | `#[derive(DjogiEnum)]`, Postgres codec, `rename_all`, per-variant overrides |
| [JSONB Fields](./jsonb.md) | `Jsonb<T>`, unknown-field preservation, flat path querying, `#[derive(JsonbSchema)]` typed paths |
| [Array Fields](./arrays.md) | `Vec<V>` columns, `contains` / `contained_by` / `overlap` / `len`, GIN index intent |
| [Full-Text Search](./fts.md) | `#[model(fts = "...")]`, generated `tsvector`, `QuerySet::search` terminals |
| [Trigram Similarity](./trgm.md) | `trgm_similar_to`, `trgm_similarity`, GIN/GiST index setup (requires `trgm` feature + `pg_trgm` extension) |
| [Spatial](./spatial.md) | `GeoPoint`, `within_km`, `order_by_distance`, PostGIS integration (requires `spatial` feature) |
| [Tenancy](./tenancy.md) | `#[model(tenant_key)]`, RLS policy emission, `set_tenant`, `_insecurely()` bypass |
| [Apps](./apps.md) | `djogi::apps!` subsystem, `#[model(app = ...)]`, retirement flow with tombstones, migration grouping |
| [Migrations](./migrations.md) | Compose / status / attune / db reset / db seed / docs commands; ledger; library APIs; classifications; out-of-order policy; PK-type flips |
| [Authentication](./auth.md) | `DjogiAuth` trait, `AuthContext`, `PasswordHash`, auto-`set_tenant`, `with_no_tenant_scope` |
| [Encrypted at Rest](./encrypted-at-rest.md) | `#[field(protected(codec = "aes256_gcm_v1"))]`, AES-256-GCM field encryption, `DJOGI_FIELD_CODEC_KEY_*` ring, rotation, threat model (requires `aes-codec` feature) |
| [Proxy Models](./proxy.md) | `#[model(proxy_for)]`, `default_filter`, `default_order`, schema-passthrough, bulk-op scoping |
| [Computed Properties](./computed.md) | `#[computed(sql = "...")]`, `{Model}Computed` ZST, `filter_expr`, Rust-trait registration |
| [Hooks and Composition](./hooks.md) | `#[model(hooks)]`, `ModelHooks`, before/after lifecycle, `#[model(auditable)]`, `#[model(soft_deletable)]`, `.not_deleted()` |
| [Secrets Hygiene](./secrets-hygiene.md) | `cargo xtask check-secrets`, allowlist markers, pre-commit and pre-issue workflows |
| [Agent Guide](./agent-guide.md) | For AI coding agents — reading Djogi code, golden path, common mistakes |
