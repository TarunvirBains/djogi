# Changelog

All notable changes to Djogi are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.17] - 2026-06-15

### Added

- feat(#442): typed CTE query builder — `QuerySet::with` and
  `QuerySet::with_recursive` compose `WITH` / `WITH RECURSIVE` preambles;
  `CteQuerySet<M>` exposes `from_cte`, `filter`, `order_by`, `limit`,
  `offset`, `cycle`, `exclude_cycle_rows`, and typed terminal methods
  (`fetch_all`, `first`, `count`, `exists`). `RecursiveArm<M>` builds
  self-referential recursive terms with a typed join edge.

### Fixed

- fix(#424): `djogi migrations verify` no longer emits contradictory D601/D602
  diagnostics for auto-generated `{parent}_outbox` tables of named-app models.
  Outbox tables are now scoped with their parent model's bucket during verify
  bucket projection. `migrate::naming::outbox_table_name` is the single source
  of truth for the `{parent}_outbox` naming convention across projection,
  verify, runtime outbox writes, and refresh polling.

## [0.1.0-alpha.15] - 2026-06-13

### Added

- feat(#355): wire `djogi migrations rollback` CLI dispatcher with
  `--to`, `--dry-run`, and `--allow-data-loss --reason`; the command
  executes committed down SQL, maps refusal/runtime exits to the shared
  migration CLI contract, and re-projects the bucket snapshot from the
  live database after committed rollback work.

### Changed

- migration: `rollback_plan` now refuses before execution when the
  applied ledger row and the current committed migration files drift on
  either checksum side.

### Fixed

- migration: `LossyRollbackPolicy::Allow { reason }` now preserves the
  operator-supplied reason even for file-derived rollback plans with no
  in-memory lossy markers, so the rolled-back ledger note keeps the
  audit trail promised by the public API.
- docs: remove stale deferral claims for the shipped migration CLI
  surface (`apply`, `verify`, `repair`, `baseline`, `rollback`, and
  `db reset`) across roadmap/spec/guide pages.
- breaking: `RollbackError::DownStatementFailed` now carries
  `live_db_committed`, and `RollbackError` exposes
  `live_db_committed()` so callers can rebuild derived state after
  post-commit rollback failures or attempted non-transactional down
  statements.
- breaking: `RollbackError` gains `ChecksumDrift { side, ledger,
  on_disk, .. }` plus the exported `RollbackChecksumSide` tag; callers
  with exhaustive matches must add the new arm. This guard assumes the
  harmonized committed-SQL checksum domain shipped in the #421 line.
- breaking: `RollbackError::Runner` changed from the tuple variant
  `Runner(RunnerError)` to the struct variant `Runner { source,
  live_db_committed }`; callers that constructed or matched it
  positionally must switch to the named-field form.

## [0.1.0-alpha.14] - 2026-06-13

### Added

- migrations: add default-on apply-time drift verification for real
  `djogi migrations apply`. Previously-applied buckets now refuse before any
  migration SQL runs when the recorded snapshot is missing or when
  error-severity drift is detected against the live catalog. `--fake` remains
  exempt and does not read the snapshot file.
- docs: add the drift-detection integration guide and wire the apply-time gate
  into the migration and configuration specs.

### Changed

- migrate API: re-export the new `DriftBaseline` enum and add
  `RunnerError::DriftDetected`, `RunnerError::DriftBaselineMissing`, and
  `RunnerError::DriftPreflightFailed` for the apply-time gate.

### Breaking

- `RunnerCtx` now requires `drift_baseline: DriftBaseline`. Real apply should
  pass the recorded snapshot state; callers that deliberately manage or skip
  verification must now say so explicitly with `DriftBaseline::Disabled`.

## [0.1.0-alpha.13] - 2026-06-13

### Added

- migration: add `canonical_fallback_replay_plan` for fallback replay plans.
- migration: no-sidecar replay fallback now uses canonical reconstruction of composed fragments so fallback-side checksums are computed from executable statements before runner verification.

### Changed

- migration: fallback replay and repair now compare against committed canonical checksum domains (`checksum_up` and `checksum_down`) derived from composed fragment SQL.

### Fixed

- migration: classify `RunnerError` outcomes for `djogi migrations apply` so
  operator-actionable refusals exit with code `2` (including out-of-order
  rejects under `Reject` policy), while retryable runtime failures remain at
  code `1`; `djogi migrations baseline` shares the same mapper.
- docs: update migration apply and fake-apply exit-code documentation in
  spec/guide guidance to match the unified mapping.
- operators: clarify the parity/reconciliation transition for fallback rows: composed legacy rows now normalize both checksum sides when repaired, and legacy hand-authored (`non-composed`) rows may stay parity-clean on `checksum_up` while exposing `checksum_down` reconciliation through `repair checksum-drift` before rollback gate #355 consumes it.

## [0.1.0-alpha.12] - 2026-06-11

### Added

- migration: `RepairError::Runner` wraps runner-level failures surfaced
  during repair — node-identity binding on the pinned resume session, the
  leaf-identity pre-check, and strict-replay plan materialization —
  preserving the full `Error::source()` chain (the underlying Postgres
  error is no longer flattened into a `LedgerIo` string). Downstream
  exhaustive matches on `RepairError` must add an arm; the CLI classifies
  it as exit 1 (retryable), unchanged from the previous behavior of every
  affected path.

### Fixed

- docs: `materialize_execution_plan` doc comment now correctly describes the
  helper as crate-internal.

## [0.1.0-alpha.11] - 2026-06-10

### Added

- migration: cross-app FK ordering within one database — same-version pending
  slices (one compose run) now apply in the dependency order recorded in each
  pending plan's `depends_on` list, derived at compose time from cross-app
  foreign-key targets. Ties break alphabetically by app label for determinism.
- migration: cross-app FK cycles are rejected at compose time with an error
  naming the participating apps; `djogi migrations compose` exits with code 2
  for this operator-actionable refusal.
- migration: `PendingLoadError::UnsupportedFormatVersion` now gives direction-
  aware recovery hints — stale pending files (found version lower than expected)
  prompt recompose; future files (found version higher than expected) prompt a
  djogi upgrade.

### Changed

- migration: pending JSON format version bumped from `"1"` to `"2"` — the new
  `depends_on` field is required; stale format-`"1"` files after upgrade are
  rejected with the version-mismatch error and must be recomposed.
- `ComposeError` is now `#[non_exhaustive]` — downstream exhaustive matches
  must add a `_` catch-all arm.

## [0.1.0-alpha.10] - 2026-06-08

### Changed

- Removed build-milestone scaffolding tokens from test function names, assertion
  string literals, fixture file names, and SQL migration comments across
  `djogi-cli/tests/`, `tests/integration/`, and `tests/internal/`.
- Reblessed `lihaaf` compile-fixture `.stderr` snapshots to align with the
  current stable Rust toolchain's diagnostic format.

## [0.1.0-alpha.9] - 2026-06-02

### Added

- feat(#381,#386): Phase Zero node identity hardening — production/cluster Phase 0 bootstrap installs HeeRanjID schema/functions without node seed or database-level GUC defaults; explicit `--single-node-dev` provisions node 1 after identity-free Phase 0 SQL succeeds. Migration CLI commands (`apply`, `baseline`, `reset`, `resume-partial`) support `--node-id <id>` and `--single-node-dev` flags. Shared Phase 0 artifact preflight allows only identity-free replay-current artifacts before replay or record paths (apply, rollback, fake apply, reset replay, repair resume, CLI cleanup); seed-capable runtime helper SQL and non-runtime top-level HeeRanjID seed-table mutations (`INSERT`/`UPDATE`/`DELETE`, CTE-led data mutations, `MERGE INTO`, `COPY ... FROM`) are refused for replay. Attune remains identity-free and refuses seed-capable, seed-DML non-runtime, ambiguous, or generated-stale Phase 0 files only for Record/Squash `--apply`. Runtime application pools remain caller-owned via `post_connect` and do NOT read `HEER_NODE_ID`.

### Changed

- migration: Phase 0 bootstrap SQL no longer contains literal `ALTER DATABASE` GUC defaults; production node identity is runner-owned through per-session binding on the pinned migration connection
- migration: selected-node reset (`--node-id` / `HEER_NODE_ID`) refuses before destructive operations because drop/create removes the old `heer_nodes` registration

## [0.1.0-alpha.8] - 2026-06-01

### Changed

- docs: rewrote the README to be adopter-facing and capability-oriented — it now describes what ships as of this release rather than the build chronology, and all repository links are absolute so they resolve from the crates.io rendering
- docs: `djogi-macros` and `djogi-cli` now publish distinct crates.io landing pages describing each crate's role, instead of sharing the workspace README
- docs: removed dev-process scaffolding (phase/cluster references, review-round and model-finding provenance) from the published crate surface — source comments, doc-comments rendered on docs.rs, diagnostic strings, and `Cargo.toml` comments now read in timeless, behavior-oriented terms; internal test and compile-fixture names were likewise made descriptive

## [0.1.0-alpha.7] - 2026-05-31

### Added

- feat(#370): adopter-linked CLI and `DescriptorProvider` boundary — adopters can drive `djogi` CLI subcommands against their own crate's models through a linked `DescriptorProvider`

## [0.1.0-alpha.6] - 2026-05-30

### Added

- feat(#369): first-class `Vec<u8>` / BYTEA model field support — `pub bytes: Vec<u8>` and `Option<Vec<u8>>` compile in `#[model]` structs; migration compose emits `BYTEA`

### Fixed

- doc: remove stale BYTEA example from `Custom` variant doc (now has its own first-class variant)
- test: add `bytea_field_sql_type_displays_as_upper_bytea` display pin test, matching the Inet/Cidr/Macaddr convention

## [0.1.0-alpha.5] - 2026-05-30

### Added

- feat(#354): wire `djogi migrations baseline` CLI dispatcher — projects live DB schema into a baseline ledger row + snapshot for existing-DB adoption

### Fixed

- fix(baseline): SnapshotPersistFailed maps to exit 2 (post-ledger-insert; retry hits VersionAlreadyApplied)
- fix(baseline): AdvisoryUnlockReturnedFalse maps to exit 2 (session-pinning correctness, matches repair family)
- doc(baseline): correct stale baseline description in docs/spec/migrations.md; update exit-code doc in main.rs and migrations.rs

## [0.1.0-alpha.4] - 2026-05-30

### Added

- feat(#353): wire `djogi migrations repair` CLI dispatcher — checksum-drift, partial-apply, resume-partial, snapshot-rebuild subcommands

### Fixed

- fix(repair): route all four repair commands to the correct per-database URL (--database flag was ignored for connection; always connected to main app DB)
- fix(repair): compute_checksum_from_disk now uses canonical fragment-level checksum domain (consistent with compose and reset; strips header and label comments before hashing)
- fix(repair): expose compute_committed_sql_checksum and compute_committed_down_sql_checksum as public API from djogi::migrate
- doc(repair): correct --app help text for checksum-drift (defaults to global bucket, not "first registered app")

## [0.1.0-alpha.3] - 2026-05-30

### Fixed

- fix(bypass): add is_identifier_byte; close $-boundary false-match in match_keyword_at (all 4 call sites: BEGIN open, END close, nested BEGIN, CASE)
- fix(bypass): extend skip_whitespace_and_match to skip -- and /* */ comments between BEGIN and ATOMIC
- test(pin): strengthen raw_ddl_begin_atomic_pin assertion; add to CI curated raw-SQL lane

## [0.1.0-alpha.2] - 2026-05-30

### Fixed

- fix(live_migrate): remove misplaced check_no_active_plan guard from run_plan; wire into compose_live_plans (CLASS A)
- fix(live_migrate): fix pool-vs-connection context confusion in daemon backfill resume; add failure persistence via record_failure (CLASS B)
- fix(live_migrate): thread allow_destructive/justify into executor; require --justify for finalize; add --allow-destructive/--justify to resume (CLASS C)
- fix(live_migrate): persist step progress via update_step_index; promote completed plans to Complete; add StepKind::as_db_str (CLASS D)

## [0.1.0-alpha.1] - 2026-05-30

### Added

- `djogi migrations verify` CLI subcommand — compares `schema_snapshot.json` against the live database catalog and reports diagnostics. Exits 0 on clean, 1 on drift or runtime error, 2 on unsupported Postgres version. Supports `--strict` to upgrade out-of-order migration warnings (D622) to errors. Read-only; does not acquire the workspace lock.
- `djogi::migrate::verify_bucket` — new library entry point for bucket-scoped verification. Uses inventory-driven app-label filtering so each `(database, app)` bucket is compared only against its own live tables, not the full `public` catalog.

### Fixed

- `djogi migrations verify` — per-database context routing: each bucket now connects to its own database target (`main`, `crud_log`, `event_log`, or user-defined) rather than always using the app database URL.
- `djogi migrations verify/status/attune` — exit code 2 (Postgres version below support floor) was incorrectly returned as exit code 1. Now correctly returns exit 2 for PG < 18, consistent with all other migration subcommands.
- `djogi migrations verify` — missing snapshot for a bucket with declared models now exits 1 with an actionable message; previously returned exit 0 (unverified state read as clean).
- `djogi migrations verify` — orphaned on-disk snapshots (apps removed from inventory) are now included in the verification pass and surface as D601 drift rather than being silently skipped.

### Added

- **Typed JSONB path cast dispatch** (closes djogi#161). Added
  `JsonbSqlCast` — a closed, non-exhaustive enum of every Postgres cast
  suffix `JsonbPathRef<M, V>` can apply — and the
  `IntoFilterValue::jsonb_sql_cast() -> Option<JsonbSqlCast>` trait
  method. Wrapper types now delegate JSONB path cast metadata to their
  inner SQL value type instead of silently falling back to text
  comparison: `primary_key!`-emitted custom PK newtypes delegate to the
  declared inner Rust type, and `ForeignKey<T>` / `OneToOneField<T>`
  delegate to `T::Pk`. A `MyAppId(i64)` field of `Jsonb<Spec>` now emits
  `(specs->>'rank')::int8 > $1` (numeric ordering) rather than the
  pre-fix `(specs->>'rank') > $1` (text ordering, where `'10' < '9'`).
  `u64` is added to the cast table as `::numeric`, matching its
  `FilterValue::Decimal` bind path. `#[jsonb(scalar)]` is added as the
  escape hatch on `#[derive(JsonbSchema)]` for adopter-defined scalar
  field types — the marker is bare-word; it accepts no SQL cast text
  (cast selection still flows through `FieldType: IntoFilterValue`).
- **Transaction retry backoff policy** (djogi#164). Added
  `TransactionRetryBackoff` and `retry_on_conflict_with_backoff` as the
  production sibling to immediate `retry_on_conflict`, with separate defaults
  for lock conflicts vs. `PoolTimeout`, capped exponential delay, configurable
  jitter, and `TransactionRetryBackoff::none()` for sleep-free tests.
- **Typed `INSERT INTO ... SELECT ...` bulk-copy surface** (closes
  djogi#106). `QuerySet<S>::insert_into::<T, _, _>(|target_fields,
  source_fields| vec![...])` returns an inert
  `InsertSelectStmt<S, T>`; `.execute(&mut ctx).await` runs the
  cross-table copy and returns the affected row count. Each column
  mapping (`target.col().copy_from(source.col().as_insert_source())`)
  pins the target column's value type AND the source operand's source
  model identity at compile time via the new
  `InsertSelectSource<S, V>` source-tagged operand and the
  `InsertSelectColumn<S, T>` doubly-tagged mapping. Constants use
  `InsertSelectSource::literal(v)` (polymorphic in `S`, inferred from
  the closure return type); arithmetic composition on
  `InsertSelectSource<S, V: Numeric>` (`+` / `-` / `*` / `/`) preserves
  the source tag. A type-erased mapping (target field as "source"
  operand, or source field as "target" column) is rejected by the
  closure-return inference, not by the runtime emitter — pinned by
  compile-fail fixtures under
  `djogi/tests/compile_fail/insert_select_*`. Target framework columns
  (`id`, `created_at`, `updated_at`) are populated by their DB
  defaults, matching `Model::create`'s contract. Tenant / RLS auto-set
  fires for both target and source. Unsupported source state
  (`prefetch`, `select_related`, `cache`, locks, distinct) is rejected
  at the terminal with `DjogiError::Validation`. Replaces the previous
  bypass-attribute-only path
  (`#[deliberately_bypass_convention_with_raw_sql]` +
  `ctx.raw_execute(...)`) for cross-table archival / migration shapes.
- **Per-scope presentation codecs** (closes djogi#227). Declare
  `#[field(protected(sensitivity = "...", rationale = "...",
  per_scope = { scope = { presentation_codec = C } ... }))]` to
  transform field values when projecting to visage scopes. Infallible
  codecs (`presentation_codec = C`) generate `From<&Model>`; fallible
  codecs (`try_presentation_codec = C`) generate `TryFrom<&Model>`.
  Built-in codecs — `Identity` (no-op), `MaskString` /
  `MaskOptionString` (mask to `[REDACTED]`), `HmacSha256HexString` /
  `HmacSha256HexOptionString` (HMAC-SHA256 hash) — cover common
  sensitive-data patterns. HMAC codecs require the `hmac-codec`
  feature flag and `DJOGI_PRESENTATION_HMAC_KEY` (64 lowercase hex characters) set
  before pool connect; `validate_startup_inventory()` is called
  automatically and surfaces any codec startup failures as
  `DjogiError::PresentationStartup`. Custom visage scopes beyond the four
  built-ins (`Public`, `SelfView`, `Admin`, `Export`) are declared
  via `#[model(visage_scopes(name = Suffix))]`. Test helper:
  `djogi::testing::install_presentation_hmac_key_for_testing(key)`.
  `DjogiPool` is now in `djogi::prelude`.

## [0.1.0-alpha.0] - 2026-05-29

Initial public alpha release. Djogi is a Model-first web framework for Rust: define
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
- Typed set operations between same-model `QuerySet<T>`s —
  `QuerySet::union` / `union_all` / `intersect` / `except` produce a typed
  `SetOpQuerySet<T>` whose `fetch_all` / `first` / `count` terminals emit
  parenthesised `(LEFT) <OP> (RIGHT)` SQL with renumbered positional binds.
  Outer `ORDER BY` / `LIMIT` / `OFFSET` apply to the combined result;
  per-arm `ORDER BY` / `LIMIT` / `DISTINCT` ride inside each arm's parens
  per Postgres rules. Chained (`a.union(b).intersect(c)`) and nested
  composition work through the sealed `IntoSetOpArm<T>` trait; arms
  carrying `.prefetch(...)`, `.select_related(...)`, locks, or `.cache(...)`
  surface a typed `DjogiError::SetOpArmInvalid` at the terminal before
  any SQL is issued.
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

[Unreleased]: https://github.com/TarunvirBains/djogi/compare/v0.1.0-alpha.14...HEAD
[0.1.0-alpha.14]: https://github.com/TarunvirBains/djogi/compare/v0.1.0-alpha.13...v0.1.0-alpha.14
[0.1.0-alpha.13]: https://github.com/TarunvirBains/djogi/compare/v0.1.0-alpha.12...v0.1.0-alpha.13
[0.1.0-alpha.2]: https://github.com/TarunvirBains/djogi/compare/v0.1.0-alpha.1...v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/TarunvirBains/djogi/compare/v0.1.0-alpha.0...v0.1.0-alpha.1
[0.1.0-alpha.0]: https://github.com/TarunvirBains/djogi/releases/tag/v0.1.0-alpha.0
