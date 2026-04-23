> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Resolved Design Decisions

---

## 19. Resolved Design Decisions

| Decision | Resolution |
|---|---|
| Default PK type | HeerId — `BIGINT DEFAULT generate_id()`, database-native, time-ordered |
| HeerId ID pattern — default | `create()` uses `DEFAULT generate_id()` + `RETURNING id` |
| HeerId ID pattern — bulk | `HeerId::generate_many(n)` pre-allocates IDs before bulk INSERT |
| HeerId ID pattern — form | `HeerId::generate()` at form render time; INSERT with `ON CONFLICT DO NOTHING` |
| Serial PKs | Opt-in via `#[model(pk = "serial")]` |
| Node identity | NODE_ID environment variable; validated against heer_nodes at startup |
| HeerId JSON serialization | Always string — enforced by HeerId serde impl, no annotation needed |
| Datetime library | `time` crate — cleaner API, no CVE history |
| Framework fields (`id`, `created_at`, `updated_at`) | Macro-injected — real fields, not written by developer |
| `create()` input | Struct itself — no separate `CreateVehicle` wrapper |
| FK loading strategy | Explicit `.fetch()` or `.prefetch()` — no lazy loading |
| M2M strategy | Explicit through models required; `ManyToMany<T>` trait for convenience methods |
| M2M method naming | Explicit `RELATION` const — auto-pluralization is error-prone |
| M2M directionality | Each direction declared separately with its own `impl ManyToMany` |
| Shell async strategy | Dedicated tokio runtime + `block_on` per call — no `.await` in shell |
| Shell transactions | `begin()` / `commit()` / `rollback()` / `savepoint()` — explicit, always rollback on exit |
| Transaction timeout | Optional, developer-chosen at `begin()` — config `transaction_timeout_default` pre-fills prompt (default: 30m) |
| Power loss / crash | Postgres auto-rollbacks dropped connections — no manual recovery needed |
| Shell error handling | Print one-liner + save full traceback to `.djogi_shell_errors/`; session never unwound |
| Shell error log retention | 1 year default; configurable via `error_log_retention` in `Djogi.toml` |
| Shell verbose mode | `cargo djogi shell --verbose` — prints full tracebacks inline in addition to saving to disk |
| Shell history | `.djogi_history` gitignored; `scripts/` committed and shareable |
| Shell import/export | `.export`, `.import`, `.bookmark` — named Rhai scripts in `scripts/` |
| Shell headless execution | `cargo djogi shell --run scripts/name.rhai` — runs script without entering REPL |
| Query engine | Djogi-owned `SqlAccumulator` + `ConditionBuilder` — no third-party query builder |
| Raw escape hatch | Always available on `DjogiContext` — `raw_query<T: FromPgRow>`, `raw_fetch_one<T: FromPgRow>`, `raw_scalar<T: FromSql>`, `raw_execute`; all take positional `$n` binds and respect the active transaction |
| CRUD log architecture | Separate `myapp_crud_logs` database; per-model mirror tables; delivery policy derived from a logging profile (`light`, `balanced`, `strict_audit`) with advanced overrides only as escape hatches |
| Event log architecture | Separate `myapp_event_logs` database; `tracing`-powered; severity-routed |
| Log database lifecycle | `db reset` wipes app DB only; `--wipe-crud-logs` / `--wipe-all-logs` for log DBs |
| Logging UX | Profile-first configuration, not a matrix of operational knobs; maintainers should choose a profile and stop |
| Cross-database logging semantics | Djogi does not promise distributed atomic commit across app, CRUD-log, and event-log databases; stricter audit behavior is enforced by policy, not by pretending the databases commit atomically together |
| CRUD log failure policy | `light` = best-effort, `balanced` = durable bounded retry with health warnings, `strict_audit` = fail-closed for required audit writes |
| Event log failure policy | Best-effort by default under all built-in profiles; outages surface as warnings and metrics, not retroactive app-write failure |
| Admin panel | Optional HTMX + Askama; `admin` feature flag; mounted at `/_admin/` |
| Admin form generation | Auto-generated from `ModelDescriptor` — zero per-model UI code required |
| Admin validation | Auto-generated from field annotations + optional `impl AdminClean` hook |
| Admin M2M inlines | Auto-rendered from `impl ManyToMany<T>`; paginated, searchable, configurable |
| Admin opt-out | Per-model `#[model(admin = false)]`; per-field `#[field(admin_hidden)]` |
| `Jsonb<T>` in admin | Nested fieldset from schema; unknown fields shown read-only with raw JSON toggle |
| Seed script transactions | `seeds.rhai` runs inside a transaction by default — partial failure rolls back entirely |
| Query builder — application code | Closure-based: `.filter(\|f\| f.field.gte(val))` — typed, composable, idiomatic |
| Query builder — shell / dynamic | Programmatic: `ModelFilter::new().field(Gte(val))` — serializable, no closures needed |
| Dirty tracking | Off by default; opt-in globally via `Djogi.toml` or per-model via `#[model(dirty_tracking)]` |
| FK cascade default | `RESTRICT` — safest Postgres default; overridden per-field with `#[field(on_delete = "...")]` |
| Field rename detection | `#[field(renamed_from = "old_name")]` — differ treats as rename not drop+add |
| Build drift diagnostic | Compiler-style `note` (not error) — migration generated, build continues, developer reviews |
| Migration generation | Automatic via `build.rs` on drift detection — generates pair, build continues |
| `makemigrations` CLI | Retained as manual trigger for `--dry-run`, `--allow-destructive`, custom naming |
| Schema snapshot | Updated only on successful `cargo djogi migrate` — reflects actual DB state, never build state |
| Migrations folder | Git submodule — pipeline-managed, invisible to developer day-to-day |
| Migration down files | Always generated as a pair; data loss on destructive rollback documented in file |
| Database target | Postgres only — permanent decision, not a limitation; enables JSONB, HeeRanjId, advisory locks, transactional DDL, `RETURNING` |
| Postgres version floor | Postgres 18 — no support for older versions. Rationale: Djogi is pre-publish and unapologetic about adoption shape; teams migrating an existing app to Djogi have substantial app-side work regardless, so bundling a Postgres upgrade is a small marginal cost. The framework will freely use any Postgres 18+ feature (extended protocol niceties, latest JSONB work, `MERGE`, logical replication, generated-column expressiveness) without version-gating fallbacks. |
| Dev database reset | `cargo djogi db reset` — gated on `dev_mode = true` + localhost URL + `DJOGI_ENV != production` |
| CLI interface | `cargo djogi` subcommand — installed via `cargo install djogi-cli`, idiomatic Rust toolchain |
| Djogi's scope | Model derivation chain only — does not duplicate SQLx, HeeRanjId, Tokio, or any Rust web framework's responsibilities. `axum` is the best-covered framework example today (opt-in via the `axum` feature flag); other frameworks integrate through their own per-framework flags or manual wiring. |
| Public requirement translation | Private app requirements may inform Djogi, but specs/docs describe them only as product-agnostic framework capabilities |
| Core vs companion crate boundary | Djogi keeps reusable data-layer primitives; domain policy, workflow logic, and specialized integrations belong in app crates or companion crates |
| `Jsonb<T>` field type | `JSONB` column with typed schema, serde deserialization, validator validation, nested schema support |
| Unknown field preservation | Fields not in schema loaded into `extra: IndexMap<String, UnknownField>` — never dropped on save |
| `UnknownField` variants | `String`, `Bool`, `Float`, `Int`, `Null`, `RawJson` — no implicit coercion between types |
| `UnknownFieldError` | `FieldNotFound`, `TypeMismatch`, `NoImplicitCoercion` — all conversions return `Result` |
| Nested `Jsonb<T>` | Fully supported — each nesting level has its own typed schema and unknown field boundary |
| JSONB subfield queries | Typed filter accessors generated per known field using Postgres JSONB path operators |
| CRUD logging | Optional per-model or global; stored in `_djogi_crud_log` table; off by default |
| CRUD log JSON diffing | Dot-notation paths through full `Jsonb<T>` nesting depth including unknown field changes |
| CRUD log actor | Optional `save_with_actor()` or request-context hook; null if not provided |
| Project scaffolding | `cargo djogi new` — scaffolds project and initializes migrations submodule |
| Regex anywhere in djogi | **Prohibited — no exceptions.** No regex-engine dependency (`regex`, `regex-lite`, `fancy-regex`, `regex-automata`, or equivalent) may enter any workspace crate (`djogi`, `djogi-macros`, `djogi-cli`, `djogi-shell`, or any future crate), **and regex notation is not permitted in doc comments, commit messages, or any other in-repo text either.** Rules are expressed in plain English and implemented with stdlib byte primitives: `u8::is_ascii_alphabetic`, `u8::is_ascii_alphanumeric`, explicit byte equality, sorted const slices with `binary_search`, stack-allocated `[u8; N]` buffers, and similar. Regex is heavy, hides intent behind a DSL most readers re-parse every time, and invites per-query allocation. Regex notation in prose is no easier to skim than the underlying rule written out — it pretends to be universal shorthand but actually requires a mental parser pass. Spell the rule out in words (e.g. "ASCII letter or underscore followed by ASCII alphanumerics or underscores, up to 63 bytes"). |
| Spatial SRID | Locked to `GEOGRAPHY(Point, 4326)` in Phase 6. Non-4326 work goes through raw SQL (`ctx.raw_execute(...)`) combined with `FieldSqlType::Custom(&'static str)` on the field type. A future `GeoPoint<const SRID: u32>` generalization is a candidate if real adoption pressure emerges — it would be an additive, non-breaking change per the pre-publish policy. |
| Spatial codec | Manual EWKB encode/decode in-repo (`djogi/src/geo/ewkb.rs`) — 25-byte little-endian layout (endian marker, type word with SRID flag, SRID, X longitude, Y latitude). No new dependency. A future dep swap to `geozero` or equivalent is a single-file change because all raw-byte logic is isolated in `ewkb.rs`. |
| Spatial feature-flagging | `spatial` feature flag on the `djogi` crate (not a separate crate, per the one-djogi-crate rule). Default builds do not compile any PostGIS surface. The `djogi::geo` module and the `djogi::GeoPoint` re-export are both gated on `#[cfg(feature = "spatial")]`. |
| Spatial query IR | `SpatialExpr` nodes live under Phase 4's `ExprNode` enum as a single `ExprNode::Spatial(SpatialExpr)` variant. No parallel `Condition::Spatial` arm. This keeps filter, order, annotate, and aggregate pipelines on one emitter walk. |
| Spatial ordering determinism | `order_by_distance` appends the primary-key column (`id`) as an unconditional ascending tiebreak. Equidistant rows sort by ascending primary key — reproducible across repeated executions and safe for keyset pagination. |
| Non-point spatial geometries (Phase 6.5) | `GeographyValue` trait is sealed; ships concrete impls for `GeoPoint`, `LineString`, `Polygon`, `MultiPoint`, `MultiLineString`, `MultiPolygon`. Each has a manual EWKB codec isolated in `djogi/src/geo/ewkb.rs` — no new dependency. OGC simple-feature invariants (closed rings, minimum-point counts) validated at constructor time; `postgres_types::{ToSql, FromSql}` impls route through the same EWKB layer. |
| Spatial shape-predicate cast discipline (Phase 6.5) | `ST_Intersects` has a native `geography` overload, so Djogi emits `ST_Intersects(<col>, $n::bytea::geography)` with no column cast. `ST_Contains` / `ST_Touches` / `ST_Within` have no geography overload in PostGIS 3.x, so both sides are cast to `geometry`: `ST_<Func>(<col>::geometry, $n::bytea::geometry)`. The `$n::bytea::<type>` double-cast is required because `Vec<u8>: ToSql` binds as `bytea`; a direct `$n::geography` cast makes `tokio_postgres` prepare `$n` as `geography` and reject the `Vec<u8>` value at prepare time. |
| Spatial region-JOIN function choice (Phase 6.5) | `group_by_region` / `count_by_region` emit `LEFT JOIN … ON ST_Covers(r.<r-geo-col>, t.<t-geo-col>)` — not `ST_Contains`. Two reasons: (1) `ST_Covers(geography, geography)` exists in PostGIS 3.x, `ST_Contains(geography, geography)` does not, so `ST_Covers` avoids `::geometry` casts under the JOIN and keeps GiST-indexed bbox prefiltering active on the geography column; (2) `ST_Covers` is boundary-inclusive (a point on a polygon's edge is "covered") whereas `ST_Contains` is interior-only (the same edge point is "not contained"). The two functions agree for points strictly inside or outside the polygon and differ only on the boundary — and for spatial grouping, the boundary-inclusive interpretation is the useful one: a point on a neighborhood edge should still count under *some* neighborhood rather than falling into the `None` bucket. |
| Spatial cluster emitter subquery wrap (Phase 6.5) | `cluster_by_proximity` wraps the `ST_ClusterDBSCAN(...) OVER ()` call in an inner `FROM (SELECT t.*, ...) AS t` subquery so the outer `GROUP BY cluster_id` references a materialised column. The flat form `SELECT ST_ClusterDBSCAN(...) OVER () AS cluster_id ... GROUP BY cluster_id` is rejected by Postgres with `ERROR: window functions are not allowed in GROUP BY`. |
| Spatial grouping source enum (Phase 6.5) | The three spatial grouping shapes (Join / Cluster / Geohash) unify under a single `SpatialGroupSource` variant on `GroupedAnnotatedQuerySet`. Dispatch to the appropriate SQL builder happens in `build_grouped_annotated_select`. New shapes are added as new variants — the grouped-substrate contract (key-tuple emission, aggregate-tuple emission, HAVING / ORDER / LIMIT tail) is identical across all three. |
| Grouped queryset type-state (Phase 6.5) | Grouping transitions through three type-states: `QuerySet<T>` → `GroupedQuerySet<T, K>` (via `group_by` / `rollup` / `cube` / `group_by_sets`) → `GroupedAnnotatedQuerySet<T, K, A>` (via `.annotate(...)`). Each stage locks in what downstream operations are allowed — you cannot fetch a `GroupedQuerySet` without first annotating, you cannot call `group_by` on an already-grouped queryset. The sealed `IntoGroupKeyTuple` and `IntoAggregateTuple` traits cover arity 1–4 in this phase; exceeding the cap raises a clear compile error. |
| Window aggregate without `group_by` (Phase 6.5) | `.annotate(...)` on a plain `QuerySet<T>` emits the aggregate with `OVER ()`, producing one row per base table row with the table-wide aggregate attached. `.annotate(...)` after `group_by` omits the `OVER` clause and emits a plain `GROUP BY` aggregate. One method, two emission modes keyed on type-state — no separate `.window(...)` entry point. |
| Aggregate alias discipline (Phase 6.5) | Aggregates are aliased `__djogi_agg_N` (N starting at 0). User-supplied SELECT aliases whose name begins with `__djogi_agg_` are rejected at SQL-build time with `DjogiError::AnnotationAliasCollision`. The synthetic alias prevents user renames from breaking positional decode. |
| `#[djogi_test(extensions = [...])]` (Phase 6.5) | The test harness macro accepts an `extensions = ["..."]` array. Each per-test database runs `CREATE EXTENSION IF NOT EXISTS "<name>"` after HeeRanjID install and before user setup. Extension names validated with a byte-level ASCII-identifier check — no regex (per the no-regex rule), no database round-trip for validation. Invalid names fail at macro expansion with a span-precise `syn::Error`; valid-but-nonexistent extensions surface as `DjogiError::Db` at runtime. |
