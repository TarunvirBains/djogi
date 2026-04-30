# Changelog

All notable changes to Djogi are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-XX-XX

Initial public release. Djogi is a Model-first web framework for Rust: define
your data schema as Rust structs and the framework derives the surrounding data
machinery — ORM, migrations, audit trail, shell bindings, JSONB schema handling,
and (opt-in) an admin console. Djogi is Postgres-only by design and targets
Postgres 18 or later. The HTTP layer is intentionally out of scope; per-framework
integrations ship behind sub-feature flags so adopters keep whichever Rust web
framework fits their app.

### Added

#### Model definition

- `#[model(...)]` attribute macro injects `id`, `created_at`, `updated_at` as
  real struct fields, implements the `Model` trait (`get` / `create` / `save` /
  `delete` / `refresh_from_db`), and emits a typed `ModelDescriptor` collected
  via `inventory` for cross-crate discovery.
- `#[model(table, pk, no_default, tenant_key, events, indexes, fts, app,
  moved_from_app, idempotency_key)]` attribute grammar for table-level options.
- `#[field(...)]` attribute grammar for column-level options including `unique`,
  `index`, `index_method`, `nulls_not_distinct`, `version`, `expose(...)`,
  `outbox`, `sequence_within`, `protect`, and per-field rationale advisories.
- Default primary key is `HeerIdRecencyBiased` — newest-first index scans
  without a secondary descending index. `HeerId`, `RanjId` (UUIDv8), the
  recency-biased variants, and `Serial` are also built-in. Custom primary keys
  (UUIDv4, ULID, Snowflake, bespoke types) plug in through the
  `djogi::primary_key!` declarative macro plus `#[model(pk = X)]`.
- `Tracked<T>` dirty-tracking wrapper emits `UPDATE … SET` clauses only for
  fields that actually changed.

#### Query layer

- Lazy `QuerySet<T>` with a typed `Condition` tree compiled to positional `$n`
  binds — nothing hits the database until a terminal method runs.
- Typed `FieldRef<M, V>` accessors with `eq` / `ne` / `gt` / `gte` / `lt` /
  `lte` / `in_list` / `contains` / `starts_with` / and the rest of the
  predicate family.
- Terminal reads — `fetch_all`, `fetch_one`, `first`, `count`, `exists`,
  `in_bulk`, plus pagination, ordering, `DISTINCT` / `DISTINCT ON`.
- Programmatic `{Model}Filter` builder and `filter_struct` for shell or
  dynamic-query use cases.
- Cursor-backed `QuerySet::stream` and `stream_with_fetch_size` terminals for
  large-result iteration without materializing every row.
- Raw SQL escape hatches on `DjogiContext` — `raw_query`, `raw_fetch_one`,
  `raw_scalar`, `raw_execute`, and `raw_stream` — always available alongside
  the typed surface.

#### Expressions and aggregates

- Typed `Expr<T>` IR covering arithmetic, field-vs-field comparisons,
  `CASE WHEN`, `EXISTS`, and correlated subqueries via typed
  `OuterRef<M, V>`.
- Aggregates — `count`, `sum`, `avg`, `min`, `max` — with `FILTER (WHERE …)`
  tails, plus `array_agg`, `json_agg`, `string_agg`, `bool_and`, `bool_or`.
- `.annotate(...)` accepts both window aggregates (`OVER ()` on ungrouped
  querysets) and grouped aggregates (`GROUP BY` on grouped querysets).
- Three-stage grouped-aggregation type-state — `QuerySet<T>` →
  `GroupedQuerySet<T, K>` → `GroupedAnnotatedQuerySet<T, K, A>` — with
  `group_by` / `rollup` / `cube` / `group_by_sets` entry points and a
  `.having(|k, a| …)` predicate slot.

#### Relations

- `ForeignKey<T>` and `OneToOneField<T>` with `OnDelete` cascade policies
  (`Restrict` default; opt-in `Cascade` / `SetNull` / `SetDefault` /
  `Protect` / `DoNothing`).
- Explicit eager loading — `prefetch(relation)` (two-query, N+1-safe) and
  `select_related(relation)` (single-round-trip `LEFT JOIN`); typed
  `{Model}Related` selectors.
- Reverse-accessor macros — `reverse_one_to_many!` and `reverse_one_to_one!` —
  emit ergonomic inherent methods on the parent.
- Explicit-through `ManyToMany<Target>` with `add_related` / `remove_related` /
  `related` and the `many_to_many!` macro for stamping each direction.

#### Field types

- `Jsonb<T>` with typed schemas, unknown-field preservation across saves, flat
  `path::<V>("dot.path")` access, and a `#[derive(JsonbSchema)]` typed
  deep-path tree with method-style accessors.
- `Vec<V>` array fields with native Postgres operators — `contains`,
  `contained_by`, `overlap`, `len`.
- `#[derive(DjogiEnum)]` typed Postgres enum codec with `#[djogi_enum(name)]`
  Postgres-type binding.
- Full-text search via `#[model(fts)]` — typed `TsVector` / `TsQuery` columns
  with materialized GIN indexes.
- Spatial types behind the `spatial` feature — `GeoPoint`, `LineString`,
  `Polygon`, `MultiPoint`, `MultiLineString`, `MultiPolygon` — all SRID 4326
  geography with manual EWKB codecs and zero new runtime dependencies.
- Spatial query surface — `within_km`, `order_by_distance` (with deterministic
  PK tiebreak), `bounded_by` (GiST-indexable bbox prefilter), `distance_to`
  (first-class `Expr<f64>`), and shape predicates `contains` / `intersects` /
  `touches` / `within`.
- Three spatial grouping entry points — `group_by_region` /
  `count_by_region` (geography-native `ST_Covers` join),
  `cluster_by_proximity` (DBSCAN), and `bucket_by_cell` (geohash precision
  bucketing).

#### Transactions and write paths

- `DjogiContext` unified execution container — pool-backed or
  transaction-backed — threads through every CRUD and queryset method.
- `atomic(executor, |ctx| async { … })` panic-safe transaction scope with
  Postgres savepoints for nested calls and FIFO `on_commit` callback drain at
  the outermost commit.
- `save(&mut self)` rehydrates the row from `RETURNING *` so trigger-driven
  column writes and `updated_at` become visible without an extra fetch.
- `#[field(version)]` optimistic locking surfaces conflicts as
  `DjogiError::LockConflict`.
- Row locks — `select_for_update`, `nowait`, `skip_locked`.
- `DjogiError::is_transient()` classification plus
  `retry_on_conflict(ctx, attempts, closure)` for retry loops over transient
  failures.
- Bulk write paths — `bulk_create`, `bulk_update`, `bulk_upsert`,
  `get_or_create`, `update_or_create`, `create_or_find`,
  `bulk_upsert_by_descriptor`.
- Scoped per-parent sequence numbering via
  `#[field(sequence_within = "parent_fk")]`.
- Transactional outbox via `#[model(events)]` — every `create` / `save` /
  `delete` inside a `DjogiContext` scope appends to a paired outbox table;
  outbox worker primitives, exponential-backoff retry, a `Publisher` trait,
  and a `pg_notify`-backed reference publisher ship behind feature flags.

#### Authentication and multi-tenancy

- Pluggable `DjogiAuth` trait — object-safe, not sealed, so `Arc<dyn DjogiAuth>`
  works at runtime and third-party providers stay first-class.
- Value-typed `AuthContext { user_id, tenant_id, scopes, ext }` with builder
  methods and scope helpers.
- `DjogiContext::with_auth` consuming builder plus `set_auth` mutating form for
  use inside `atomic()` closures; nested-atomic auth-state snapshot/restore
  guarantees rollback semantics.
- `PasswordHash` typed column with transparent codec and an Argon2id hasher
  behind the `auth-argon2` feature flag — constant-time `verify` returns
  `bool` so timing leaks stay shut.
- Multi-tenancy via `#[model(tenant_key = "...")]` — emits Postgres RLS
  policies and binds them to a per-context tenant id; every CRUD or queryset
  op auto-issues `set_config('app.tenant_id', …, true)` before execution.

#### Visages

- Per-`#[model]` projection types — `{Model}Public`, `{Model}SelfView`,
  `{Model}Admin`, `{Model}Export` — derived automatically with unconditional
  `serde::Serialize` / `Deserialize` and field-level opt-in via
  `#[field(expose(...))]`.
- `expose(...)` grammar covering scalar form, relation form
  (`expose(public = "PeerVisage")`), and `->` traversals (ID-only, narrow
  peer, full-struct with nested exposure).
- Uniform peer `TryFrom<&Source>` conversion via the stdlib `From` blanket
  plus an `Infallible → VisageError` bridge — one call site for both scalar
  and relation peers.
- First-class visage query surface — every visage gets its own `filter(...)`
  entry, `{Visage}Fields` accessor type, SELECT-narrowing emission (only the
  exposed columns hit the wire), and compile-time FK / reverse-FK / M2M
  boundary enforcement via the sealed `DjogiVisageOf<M>` trait.

#### Apps subsystem

- `djogi::apps! { #[app(database = "main")] pub struct Vehicles; }` macro
  registers application boundaries; `#[model(app = Vehicles)]` binds models to
  apps via type paths (not string labels).
- `AppRegistry::all()` runtime registry keyed on `(database, label)` identity
  with a synthetic global bucket for unbound models.
- Lifecycle markers — `#[app(renamed_from = "old")]` and `#[app(tombstone)]` —
  with compile-time guards against active models on tombstoned apps and a
  `#[model(moved_from_app = OldApp)]` migration helper.
- `AppRegistry::cross_app_edges()` and `cross_app_cycles()` walk the FK graph
  for cross-app referential integrity audits.

#### Migrations

- Descriptor-driven differ — every model's `ModelDescriptor` projects into a
  `MigrationShape`; the differ compares the projected shape against the
  committed schema snapshot and produces a typed `MigrationPlan`.
- Classification ladder — `NoOp`, `Additive`, `Reversible`, `Destructive`,
  `Lossy`, `Unsupported`, `PkTypeFlip` — with severity ordering and
  `LossyRollbackPolicy::Allow { reason }` opt-in for lossy rollbacks.
- Online-safety classification dimension — `OnlineSafe`,
  `FastLockDestructiveGuarded`, `ExpandContract`, `OfflineOnly` — layered on
  top of severity so live-row impact is visible at plan time.
- `IndexSpec` v3 contract covers unique constraints vs unique indexes,
  expression-target indexes, partial indexes, covering (`INCLUDE`) indexes,
  `NULLS NOT DISTINCT`, `CONCURRENTLY`, and required Postgres extensions —
  all surfaced through `#[field(unique, index, …)]` and
  `#[model(indexes(...))]`.
- EXCLUSION constraints (`#[model(exclusions(...))]`) and stored generated
  columns (`#[field(generated = "expr")]`) at the descriptor and snapshot
  layer.
- Live-migration substrate — backfill engine with chunk-loop SQL pattern,
  plan resume, daemon-mode runner, and protected-data audit hooks via
  `#[field(protect = "...")]`.
- Schema snapshots — `migrations/<database>/<app>/schema_snapshot.json`
  written atomically after every transactional segment commits.
- Build-time drift detection — `build.rs` reads `target/djogi_models.json`,
  diffs against the committed snapshot, and emits `cargo:warning=` lines
  classified into Outcome 1–4 (no migrations are written automatically).

#### CLI

- `cargo djogi migrations compose [--name] [--allow-destructive]
  [--force-overwrite]` — emits `V<14-digit-ts>__<slug>.sql` plus `.down.sql`
  per `(database, app)` bucket.
- `cargo djogi migrations status` — read-only ledger inspection.
- `cargo djogi migrations attune [<git-target>] [--apply] [--record-ledger]
  [--squash --from <ver>]` — three-mode reconciliation (DiffOnly / Record /
  Squash).
- `cargo djogi db reset --yes [--maintenance-database <name>]` — triple-gated
  on localhost, non-production, and explicit `--yes`. Logging databases
  survive.
- `cargo djogi db seed [--database <name>]` — idempotent via
  `djogi_seed_runs` ledger.
- `cargo djogi docs [--output <path>]` — byte-deterministic Markdown reference
  for every registered `#[model]`.

#### Substrate and tooling

- `tokio-postgres` + `deadpool-postgres` + `postgres-types` substrate —
  `DjogiPool` wraps the deadpool surface; `DjogiContext` dispatches over
  pool-backed and transaction-backed connections without leaking which is in
  use.
- `#[djogi::djogi_test]` integration-test harness — per-test ephemeral
  Postgres database, HeeRanjID schema install, default-node seed,
  `heer.node_id` setup, and optional `extensions = [...]` provisioning, all
  with zero per-test boilerplate.
- `#[djogi_test(sync_models = [Model1, Model2])]` materializes the listed
  models without ledger / advisory-lock / classification gating and routes
  through the same projection pipeline the production runner uses.
- Workspace-lock primitive — `WorkspaceGuard` typed witness — keeps two
  concurrent runners from racing on the same `migrations/` tree.

### Security

- Row-level security via `#[model(tenant_key = "...")]` — emits Postgres RLS
  policies, binds them to `set_config('app.tenant_id', …, true)` issued by
  every CRUD and queryset op, and tracks the applied tenant id per context so
  auth changes mid-transaction re-issue `SET LOCAL` correctly.
- Searchable opt-out — `_insecurely()` suffix methods on tenant-keyed models
  emit `tracing::warn!` with `#[track_caller]`-captured caller location and
  internally issue `SET LOCAL row_security = off`. Invocations are
  grep-able in production logs.
- Password hashing via `PasswordHash` typed column behind the `auth-argon2`
  feature — Argon2id with reasonable defaults; constant-time `verify`
  returns `bool` to prevent timing leaks; empty PHC sentinel always rejects.
- Audit trail via the transactional outbox (`#[model(events)]`) — every
  `create` / `save` / `delete` inside a `DjogiContext` scope appends a typed
  payload to the paired outbox table atomically with the data write, so
  audit rows can never desync from the underlying state.
- Field-level protected-data attributes — `#[field(protect = "...")]` and the
  `phf`-backed `field_codec` registry — wire sensitive columns into the
  live-migration substrate's protected-data audit path.

[Unreleased]: https://github.com/TarunvirBains/djogi/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/TarunvirBains/djogi/releases/tag/v0.1.0
