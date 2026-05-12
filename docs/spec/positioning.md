> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Positioning — Djogi in the Rust Data Tier

**Last verified:** 2026-05-10
**Refresh cadence:** spot-check upstream feature lists at each minor version (currently pre-v0.1.0; first scheduled re-verification immediately precedes the v0.1.0 publish gate).
**Source draft baseline:** the migration-only matrix at [`../research/migrations/2026-04-22/topics/12-rust-ecosystem-contrast.md`](../research/migrations/2026-04-22/topics/12-rust-ecosystem-contrast.md) and the broader 2026-05-09 working draft.
**Companion:** the [`ReadMe.MD`](../../ReadMe.MD) opening sets Djogi's design-north-star and what Djogi owns versus delegates; this doc widens the lens to where Djogi sits relative to other Rust data-tier projects so adopters can decide quickly.

---

## How to read this doc

This is a positioning doc, not a competitive-takedown doc. Djogi stands on its own choices — a Postgres-native, Model-first data engine targeting Postgres 18 and later, with Tokio as the async runtime and HeeRanjId for primary keys. The matrix below catalogues what Djogi ships and how the surface compares to other Rust data-tier projects an adopter might evaluate. Where a row reads "first-class" for Djogi and a different mode for another project, treat that as a description of design intent, not a verdict — every project in the matrix made deliberate trade-offs that fit different audiences.

If you came here from the README's design-north-star section, the short version is: Djogi treats the data tier as the primary derivation target. The long version of why that pays off, and what Djogi gives up to commit to it, is what the rest of this doc maps out.

---

## 1. Capability matrix

| Dimension | **Djogi v0.1.0** | **Cot** | **SeaORM** | **Diesel** |
|---|---|---|---|---|
| Async model | Tokio-only; unified `DjogiContext` pool/txn | Tokio-only via `sea-query-binder` | Tokio (active); async-std runtime flag still present but deprecated upstream | Sync core; `diesel-async` separate addon |
| Postgres driver | `tokio-postgres` + `deadpool-postgres` | sqlx (via sea-query-binder) | sqlx | Diesel-internal libpq wrapper |
| Pool | `deadpool-postgres`; typed `DjogiPool::builder()` | sqlx::Pool | sqlx::Pool | r2d2 (sync) |
| Macro style | `#[model(...)]` + `#[field(...)]` attributes | `#[model]` + `#[migration_op]` attributes | `#[derive(DeriveEntityModel)]` derives | `table!` declarative |
| Query type-safety | Typed `QuerySet<T>` + public `Q<T>` algebra + `FieldRef<M, V>` | Builder API; const struct migrations | SeaQuery builder | `table!`-driven `QueryDsl` |
| Migration generation | Descriptor-driven differ; `build.rs` drift warning; CLI `djogi migrations compose` | CLI `cot migration make` (AST diff) | `sea-orm-cli generate entity`; migrations hand-written | Manual SQL; `diesel print-schema` |
| Migration file format | SQL up/down + `#[migration]` Rust escape hatch | Rust code (generated) | Rust trait impl OR raw SQL | Paired SQL files |
| Online-safety classification | 4-tier: OnlineSafe / FastLockDestructiveGuarded / ExpandContract / OfflineOnly | Not surfaced | Not surfaced | Not surfaced |
| Live/staged migration | Phase 7.5 substrate: expand-contract, protected-field codecs, chunked backfill, daemon-mode runner | Not surfaced | Not surfaced | Not surfaced |
| Multi-DB | Postgres-only (permanent design choice) | PG / MySQL / SQLite | PG / MySQL / SQLite | PG / MySQL / SQLite |
| Multi-tenancy / RLS | First-class via `#[model(tenant_key)]` + auto `set_config()` | Not surfaced | Not surfaced | Not surfaced |
| JSONB typed schemas | `Jsonb<T>` + `#[derive(JsonbSchema)]` deep-path accessors | Untyped | `serde_json::Value` only | Untyped |
| Spatial | PostGIS behind `spatial` feature; GeoPoint/Line/Polygon/Multi* with EWKB codecs; `convex_hull` / `intersection` / `area` aggregates | Not surfaced | sqlx types only | Not surfaced |
| Full-text search | `#[model(fts)]` → `TsVector`/`TsQuery` + GIN index | Not surfaced | Not surfaced | Not surfaced |
| Audit / outbox | `#[model(events)]` transactional outbox + Publisher trait + pg_notify listener | Not surfaced | Not surfaced | Not surfaced |
| Field-level protection | `#[field(protect = "...")]` codec registry for PII / encrypted-at-rest | Not surfaced | Not surfaced | Not surfaced |
| Aggregates | count / sum / avg / min / max + array_agg / json_agg / string_agg / bool_and / bool_or with `FILTER (WHERE)` | SeaQuery-mediated | SeaQuery-mediated | Typed DSL (`sum`/`avg`/`min`/`max`); no `FILTER (WHERE)` |
| Window functions | `Expr<T>::over(Window)`, RowNumber / Rank / DenseRank + `.qualify()` | Limited via SeaQuery | Limited via SeaQuery | Typed DSL (`rank`/`row_number`); no `.qualify()` |
| Recursive CTEs / tree queries | `RecursiveQuerySet<T>::tree_descendants` / `tree_ancestors` with cycle detection, BREADTH / DEPTH FIRST, path output | Hand-written SQL | Hand-written SQL | Hand-written SQL |
| GROUPING SETS / ROLLUP / CUBE | First-class via three-stage grouped state | Not surfaced | Not surfaced | Not surfaced |
| Relations | `ForeignKey<T>` / `OneToOneField<T>` / `ManyToMany<Target>` with cascade policies; reverse-accessor macros; `select_related` / `prefetch` | FK only; limited reverse | FK only; limited reverse | FK only; no reverse macros |
| ENUM | `#[derive(DjogiEnum)]` Postgres-native codec | SeaQuery builder | SeaORM derive | Hand-mapped |
| Raw SQL escape hatches | `raw_query` / `raw_fetch_one` / `raw_scalar` / `raw_execute` / `raw_stream` on `DjogiContext`, gated by an explicit bypass attribute | Not typed | Standard practice | Standard practice |
| Admin / shell / CLI | `cargo djogi docs`, `cargo djogi db seed`; Rhai shell deferred Phase 9 | Built-in `cot::admin` module + `cot-cli` | SeaORM Pro (official add-on) | Not surfaced |
| Model hooks | `#[derive(ModelHooks)]`, zero-overhead via marker trait | Not surfaced | Event listeners | Not surfaced |
| Computed fields | `#[computed(sql = "...")]` + `#[djogi::trait_impl]` cross-type registry | Not surfaced | Not surfaced | Not surfaced |
| Proxy models | `#[model(proxy_for = "Parent")]` w/ custom default-filter | Not surfaced | Not surfaced | Not surfaced |
| Query algebra (public) | `Q<T>` enum w/ `&`, `\|`, `^` (XOR), `!` overloads; `.exclude_struct()` | Internal | SeaQuery (no XOR) | Not surfaced |
| Cache integration | `.cache(&punnu)` + `QuerySet::refresh_into` delta-sync (sassi) | Not surfaced | Not surfaced | Not surfaced |
| Typed projections / partial-model views | Full: per-projection `{Proj}::filter()` + Fields/Filter accessors, SELECT narrowing, FK/M2M traversal via `expose(...)`, sealed `DjogiVisageOf<M>` boundary (Djogi calls these *visages*) | Minimal — no typed projection surface | Partial: `#[derive(FromQueryResult)]` + custom select gives a typed result shape; no per-projection filter API or relation traversal | Partial: tuple select (`select((id, name))`) is typed but unnamed — no field methods on the projection, no relation surface |
| PK extensibility | `PrimaryKey` / `PrimaryKeyDbGen` / `PrimaryKeyClientGen` traits + `djogi::primary_key!` macro | Macro-based | Hand-written | Hand-written |
| Apps subsystem (schema-domain partitioning) | `djogi::apps!` + `AppRegistry` with `renamed_from` / `tombstone` lifecycle | Apps concept supported | Not surfaced | Not surfaced |
| Bulk ops | `bulk_create` (pre-allocated PKs), `bulk_update`, `bulk_upsert` with dirty-tracking | Limited | Limited | Hand-written multi-row INSERT |
| Transactions | `atomic()` w/ savepoints + FIFO `on_commit` callbacks + RLS / tenant scope snapshot/restore | Explicit txn API | Explicit txn API | Explicit txn API |
| MSRV | 1.95+ (2024 edition) | Pre-publish | Pre-publish re-verify | Pre-publish re-verify |

### Terminology notes

The matrix labels use generic concepts so each row reads on its own merits; Djogi's internal vocabulary is glossed in the cell text:

- **Typed projections / partial-model views** — Djogi calls these *visages*. The generic concept covers any framework's notion of a partial-model surface that returns a different Rust type than the source model. Djogi's distinguisher is the *named* typed surface that carries its own filter API, relation traversal, and a sealed compile-time boundary, rather than an ad-hoc tuple or a result-only struct.
- **Apps subsystem (schema-domain partitioning)** — both Djogi and Cot use the term "apps". Djogi's variant adds `renamed_from` / `tombstone` lifecycle markers and cross-app FK graph validation.
- **Cache integration** — Djogi's implementation is via *Punnu* (sassi). The cell names the module the same way SeaORM's cell names "SeaORM Studio".

---

## 2. Where Djogi sits relative to other Rust data-tier projects

This section reads each project on its own design intent first, then names the axes where Djogi's intent diverges.

### Relative to Cot

**Cot's design intent:** Cot positions itself as a full Rust web stack — model, migrations, admin, and view layer in one. Its app concept maps domain partitions across the whole framework.

**Where Djogi extends the surface:** staged-rollout machinery (Phase 7.5 live-migration substrate); tenancy / RLS first-class; JSONB typed schemas; spatial; FTS; advisory locking with checksums and replica-safety; typed projection surface with compile-time boundary; recursive CTEs with path output; public `Q<T>` algebra with XOR; Punnu cache integration; protected-field codecs; computed fields plus a cross-type trait registry; proxy models.

**Where Cot extends the surface:** AST-driven autogeneration handles more heuristic edge cases at the migration boundary; an admin pattern ships with the framework (Djogi's admin is the [Maahi console](./maahi/index.md), opt-in via the `admin` feature and currently sequenced for Phase 10).

### Relative to SeaORM

**SeaORM's design intent:** SeaORM optimises for multi-database portability and an ergonomic builder API on top of SeaQuery, with a third-party Studio for an admin-style surface.

**Where Djogi extends the surface:** the entire right side of the matrix — CTEs / recursive / window / GROUPING SETS / ROLLUP / CUBE first-class; online-safety classification; live migration; spatial / FTS; protected fields; tenancy / RLS; model hooks; computed fields; proxy models; named typed projection surface (Djogi's *visages* go beyond a result-only `FromQueryResult` derive); apps subsystem.

**Where SeaORM extends the surface:** multi-DB at runtime (PG / MySQL / SQLite); larger community and more third-party integrations; the `FromQueryResult` partial-model derive is simpler to reach for than Djogi's full visage system when an adopter only needs a one-off result shape.

### Relative to Diesel

**Diesel's design intent:** Diesel is the longest-standing Rust ORM, prioritising minimal abstraction overhead and a strongly-typed `schema.rs` codegen path that gives compile-time guarantees down to the column.

**Where Djogi extends the surface:** async-first; descriptor-driven migrations with classification plus live-migration staging; every advanced query (CTEs / tree / window / aggregate / FTS / spatial); tenancy / RLS; typed `Q<T>`; JSONB and arrays first-class with native operators; named typed projections (Djogi's *visages* go beyond Diesel's tuple select); model hooks; typed `DjogiPool` builder; protected-data codecs; bulk ops with pre-allocation and dirty-tracking.

**Where Diesel extends the surface:** longest-standing Rust ORM with a long battle-test record at scale; the `schema.rs` codegen offers a different and arguably crisper compile-time guarantee than descriptor JSON; minimal abstraction overhead with direct SQL emission.

---

## 3. async-std support — the standing decision

**Djogi is Tokio-only and intends to stay that way.** async-std has been effectively dormant in the Rust async ecosystem since approximately 2023; runtime convergence on Tokio is essentially complete. Djogi's foundation is Tokio-bound all the way down — `tokio-postgres` (literally), `deadpool-postgres`, the notify / listener path. Adding async-std would mean either:

- swap driver to sqlx (substantial regression on the runner-control story Phase 7 was built around — advisory locking, checksum enforcement, COPY / streaming control), or
- build a runtime-abstraction layer (every async trait feature-gated, doubled CI matrix, doubled bug surface, doubled test rigor).

Neither buys a meaningful audience in 2026. The honest "runtime portability" answer would be SeaQuery's path — be runtime-agnostic at the IR layer and let consumers pick — which is a far more invasive rearchitecture than "add async-std" and outside v0.1.0 scope regardless.

The decision: keep Tokio-only as a permanent design choice, alongside Postgres-only, and document it in the v0.1.0 README's design-decisions section.

---

## 4. The two axes: Rust-first and Postgres-first

Djogi's design intent is explicit: **Rust-first** (idiomatic Rust where user code touches, leverage the type system, zero-cost abstractions) and **Postgres-first** (target Postgres 18+ features without lowest-common-denominator compromises). This section grades Djogi against that intent.

### Rust-first scorecard

| | **Djogi** | **Cot** | **SeaORM** | **Diesel** |
|---|---|---|---|---|
| Source of truth is a Rust struct | Yes — `#[model]` attribute injects fields into the struct | Yes — `#[model]` attribute | Mixed — `entity` crate codegen-from-SQL, schema-first rather than model-first | Yes — `table!` declarative macro |
| Type-state in query layer | Strong — `QuerySet<T>` → `GroupedQuerySet<T,K>` → `GroupedAnnotatedQuerySet<T,K,A>`; sealed `DjogiVisageOf<M>`; `HasHooks` marker; `Tracked<T>` | Less type-state-driven | SeaQuery is SQL-shaped, less Rust-shaped | Strongest compile-time guarantees in the ecosystem |
| Zero-overhead abstractions | Yes — marker-trait monomorphisation; `#[derive]`-driven codegen; no runtime reflection | Comparable | Mixed — async-trait and dyn dispatch in places | Yes — no runtime cost |
| Async-Rust idiom alignment | Yes — Tokio-native | Yes — Tokio-native | Mixed — Tokio is the active runtime path; the async-std runtime flag persists but is deprecated upstream | Sync-only core; an async addon ships separately |
| Public algebra in Rust types (not strings) | Yes — `Q<T>` enum w/ `&` `\|` `^` `!` overloads; bare-ident PK syntax (no string PK names) | Internal condition tree | Builder-style API | No public algebra |
| Typed JSONB / arrays / spatial | Yes — `Jsonb<T>` + `#[derive(JsonbSchema)]`; `Vec<V>` w/ native operators; EWKB-typed | Untyped | `serde_json::Value` only | Untyped |
| Cross-type trait registry | Yes — `#[djogi::trait_impl]` + `Sassi::all_impl::<Trait>()` | Not surfaced | Not surfaced | Not surfaced |

**Concessions where Djogi steps away from the strict Rust-first ideal:**

- **Descriptor JSON at rest** — but it is a build artifact regenerated from Rust each compile; the source of truth *is* Rust.
- **SQL migration files** — chosen for operator transparency, replica safety, and DBA-readability over Rust-code migrations. The `#[migration]` Rust escape hatch covers data-only operations. Cot's choice on this axis is more strictly Rust-first (migrations *are* Rust code there).
- **`inventory` crate global registry** — relies on linker behaviour; a pragmatic concession for cross-crate model discovery.

**Verdict (Rust-first):** intent largely achieved. Two real concessions (descriptor JSON, SQL migrations); both pragmatic with documented motivation. On async alignment, typed JSONB / spatial, and `Q`-algebra Djogi sits at the leading edge of the surface area.

### Postgres-first scorecard

| | **Djogi** | **Cot** | **SeaORM** | **Diesel** |
|---|---|---|---|---|
| Single-DB target | Yes — Postgres-only, permanent | Multi-DB | Multi-DB | Multi-DB |
| Postgres-native recursive (`CYCLE … USING path`, `SEARCH BREADTH/DEPTH FIRST BY`) | Yes | LCD-bound | LCD-bound | LCD-bound |
| Advisory locks | Yes — per-target SHA-256 lock keys | Not surfaced | Not surfaced | Not surfaced |
| RLS / `set_config()` / tenant_key | Yes — first-class | Not surfaced | Not surfaced | Not surfaced |
| JSONB native operators | Yes — deep-path | Not surfaced | Via `serde_json::Value` | Not surfaced |
| FTS (`tsvector` + GIN) | Yes — `#[model(fts)]` | Not surfaced | Not surfaced | Not surfaced |
| PostGIS / EWKB | Yes — `spatial` feature plus spatial aggregates | Not surfaced | sqlx types | Not surfaced |
| `NULLS NOT DISTINCT`, partial / covering / functional indexes, `CREATE INDEX CONCURRENTLY` | Yes — all first-class | Not surfaced | Not surfaced | Not surfaced |
| `GROUPING SETS` / `ROLLUP` / `CUBE` | Yes — first-class | Not surfaced | Not surfaced | Not surfaced |
| Transactional outbox + `pg_notify` | Yes — `#[model(events)]` + Publisher trait | Not surfaced | Not surfaced | Not surfaced |
| Window fns + `.qualify()` | Yes — typed surface | Via SeaQuery | Via SeaQuery | Typed window helpers; no `.qualify()` |

**Verdict (Postgres-first):** intent achieved emphatically. Postgres-first is Djogi's most uncompromised design axis. Multi-DB projects forgo this depth by design — that is the trade they made for portability. Postgres-native `CYCLE … USING path` is a clean illustration: Djogi can use it because it does not have to abstract over MySQL's missing recursive-CTE ordering or SQLite's recursive-CTE feature gates.

### The two axes reinforce each other

The most important architectural finding: **Rust-first and Postgres-first are complementary, not orthogonal.** Postgres-only freedom lets Rust's type system fully model Postgres's feature set without LCD compromise — typed JSONB, typed PostGIS, typed window-fn modifiers per aggregate category, four-tier online-safety classification with PG-specific destructive-lock semantics. A multi-DB Djogi could not have been as Rust-first because the type machinery would have to abstract away PG-specific behaviours.

The mirror-image observation also lands: multi-DB Rust data projects that aim for Rust-first surface ergonomics still must flatten the Rust types to portable SQL semantics. That is precisely why typed JSONB, typed FTS, typed spatial, RLS, GROUPING SETS, and Postgres 18 recursive-CTE machinery don't surface in those projects — the Rust types can't model what the abstraction has to hide. This is not a flaw in those projects; it is the cost of the portability they chose. Djogi made the opposite trade.

### Places to double down further (post-v0.1.0)

- **Typed `EdgeName` enum for tree-query paths** (currently `Vec<String>`) — pure Rust-first improvement; deferred v0.2.
- **Field-accessor property-style API** (#138) — a more compact accessor surface than the current closure form. Rust-first.
- **Postgres 18 `MERGE … RETURNING`** typed surface — Postgres-first envelope extension.
- **Logical replication slots / publication management** — Postgres-first extension into the operational surface.
- **Column-level statistics targets** (`ALTER COLUMN SET STATISTICS`) — declarable via attribute; Postgres-first.

---

## 5. Open / unresolved (post-v0.1.0)

Routed to post-v0.1.0 with issue numbers where available:

- **#138** — field accessor property-style API (`.field_name` syntax deferred).
- Stored computed columns — `#[computed(sql = "...", stored)]` deferred (Postgres irreversibility).
- Adopter-defined field-group derives — built-in `#[derive(Auditable)]` / `#[derive(SoftDeletable)]` ship today; a public extension trait letting adopters compose new field-groups (e.g., `Versioned`, `Approved`) is deferred 8.5+.
- Constraint / index name interpolation — pattern-substitution ownership ambiguous; deferred 8.5.
- Distributed placement / residency — no node affinity, shard routing, or topology semantics in v0.1.0 (Phase 11+).
- Cross-target FK moves — no first-class pattern; classified `OfflineOnly`.
- Typed `EdgeName` enum — tree-query paths use `String` edge names; typed enum deferred v0.2.
- Lifecycle plan / apply governance — no approval workflow for migrations; deferred Phase 9.5.
- OpenAPI schema export — `djogi schema --format openapi` deferred Phase 9.

**Performance smoke-benchmarked, not yet perf-guaranteed (publish-gate analysis pending in Phase 8.5 Cluster C/D):**

- materialized-closure scalability at 5000+ nodes (smoke bench: `tests/integration/phase8_zero_tree_query_bench.rs`);
- `array_append` cost for path accumulation (smoke bench: `tests/integration/phase8_zero_tree_query_bench.rs`);
- window-function performance with 1000+ rows (smoke bench: `tests/internal/sources/phase8_zero_cluster_c_bench.rs`);
- pool builder overhead at 64+ concurrent acquirers (smoke bench: `tests/internal/sources/phase8_zero_pool_bench.rs`).

---

## 6. Verification methodology

This section exists so any reviewer can re-run the verification before publish.

**What was verified directly (very high confidence):** all Phase 7–8 shipped features, against the in-repo CHANGELOG, plan files, and source. Migration system architecture, online-safety classification, live-migration machinery, tree queries, spatial, window functions, hooks, composition, `Q`-algebra.

**What was synthesised (high confidence):** feature deltas across phases, phase sequencing, phase plan synthesis. Sources: `docs/spec/implementation-plan.md` and `docs/spec/decisions.md`.

**What is dated and needs re-verification near the v0.1.0 publish gate (medium confidence):**

- Punnu integration exact API surface — sassi v0.1.0-alpha.2 is published; final v0.1.0 pending.
- Zero-quarantine guard implementation — spec is locked, the guard has not yet run end-to-end.
- Cot's exact projection-surface state — verified absent in inspection but Cot is pre-1.0 and moves; spot-check upstream main before publish.
- SeaORM and Diesel feature matrices — each cell that claims uniqueness (RLS, FTS, spatial, tree queries, window + qualify, computed fields, proxy models, typed projections) should be spot-checked against current upstream main of each project before publish. A row that flips from "not surfaced" to "shipped" between this date stamp and the publish gate should be reflected in this doc, not silently left stale.

**What is not verified (low confidence — flagged):**

- Distributed placement / residency semantics (Phase 11+).
- Long-term migration scaling beyond 5000-node fixture.
- Absolute-latest Cot / SeaORM / Diesel main-branch state (matrix reflects 2026-04-22 baseline plus targeted spot-checks).

**Re-verification protocol near publish:**

1. Re-read each project's most recent CHANGELOG / release notes.
2. For each "not surfaced" cell where Djogi claims uniqueness, search the project's repository for the relevant feature surface and confirm absence; if a feature shipped, update the cell.
3. Bump the **Last verified** date stamp at the top.
4. Spot-check Djogi's own claims by name (`#[model(fts)]`, `#[derive(ModelHooks)]`, `tree_descendants`, etc.) against the in-repo source — claims that no longer compile against current `main` get updated, not retained.

---

## 7. Naming bridge — visages

The matrix uses the generic term *typed projections / partial-model views* in row labels so each row stands on its own. In Djogi's source, docs, and the rest of the spec, the same surface is called *visages*. First-mention bridge for adopters reading the spec or the source for the first time: a *visage* is a typed projection — a sub-shape of a model with its own filter API, relation traversal, and a sealed compile-time boundary. See [`docs/spec/visages.md`](./visages.md) for the full surface.
