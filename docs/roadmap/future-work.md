> [Back to roadmap index](./index.md) | [Back to README](../../ReadMe.MD)

# Djogi Roadmap — Future Work

Items surfaced during planning that do NOT fit the existing phase sequence. Not scheduled for any specific phase; not blockers for shipping. Captured here so the design intent survives session boundaries.

Added 2026-04-20 during Phase 5-Zero planning.

---

## 1. Request pipelining exploitation

**What it is.** The Postgres extended-query protocol lets a single connection hold multiple queries in flight concurrently — issue N `client.query(...)` futures, await them via `tokio::try_join!`, all run on one connection without serializing. `tokio-postgres` supports this natively; sqlx never did.

**Why this is here.** Phase 5-Zero T2 (merged) installs the tokio-postgres substrate, which makes pipelining *available* in Djogi. T2 does NOT rewire any existing QuerySet terminal to pipeline by default — `.fetch_all().await` stays a single awaited round-trip. Actual exploitation is future work.

**Workloads that benefit:**

- **Prefetch batching.** Phase 3's `select_related` / `prefetch_related` currently fetch the outer set, then fetch each related set sequentially. Pipelined, the related-set queries fire concurrently on the same connection. Win scales with the depth of the prefetch chain.
- **Bulk writes.** Multiple `INSERT` / `UPDATE` statements can be fired back-to-back without per-statement round-trip latency.
- **Geospatial queries (particularly).** Spatial lookups commonly pair a fast bounding-box prefilter with an exact point-in-polygon or KNN test. Today those run serially; pipelined, the prefilter and the exact check fire concurrently and the client correlates results in the same latency budget. Similar wins for bbox + distance sort, or multi-layer intersection queries. Worth baking into the spatial (PostGIS) feature flag's API from the start — when that phase's detailed plan gets written, the spatial query surface (`within_km`, `order_by_distance`, multi-layer intersection) should be designed to admit pipelined execution rather than sequential round-trips.
- **`COPY FROM STDIN`** streaming on one connection while other pool connections service regular queries.
- **`LISTEN` / `NOTIFY`** on a subscribed connection concurrent with query traffic.

**What "exploitation" means concretely.** Changes to Djogi's internal fetch paths (relation stitching, aggregate emission, bulk-write codegen) to issue pipeline-able queries and await them in parallel where correctness allows. Some of these changes compose cleanly with Phase 5-One's native-protocol features; others (like geospatial pipelining) land when the `spatial` feature flag work happens.

**Scheduling.** Not a phase of its own; folds into the feature phases that depend on it. Phase 5-One's "bulk writes" and "COPY ingest" items are the first natural consumers; the spatial feature flag picks it up when that phase arrives.

---

## 2. Djogi-native compile-time SQL validation macro

**What it is.** A proc-macro `djogi::query!(ctx, "SELECT ... FROM <table> WHERE ...")` that:

- Reads `target/djogi_models.json` (already written by `#[model]` via the existing `build.rs` pipeline)
- Validates the SQL string against known tables, columns, and their Postgres types at compile time — without needing a live database connection
- Emits a `ctx.raw_query::<T>(sql, &[binds])` call with the inferred return type

The result is sqlx-style compile-time safety for hand-written SQL that Djogi's QuerySet DSL doesn't cover (window functions, CTEs that don't fit the DSL, exotic Postgres syntax) — without depending on sqlx at all.

**Why it's not in the existing phases.**

- Djogi's QuerySet DSL already gives compile-time safety for the vast majority of queries (the type system catches wrong column names, wrong types, missing joins).
- `ctx.raw_query` (Phase 5-Zero T5) is the native escape hatch for hand-written SQL — parameterized, typed on the return side, runs inside `atomic()`.
- Users who want sqlx-style validation for their hand-written SQL today can add `sqlx` to their own Cargo.toml alongside Djogi — two pools against the same DB, independent of framework boundaries. A Djogi-bundled feature flag isn't needed for users to get this capability.
- Building the macro is non-trivial (SQL parser that understands Postgres-flavor SQL well enough to resolve column references + type-check expression trees against `FieldDescriptor` metadata).

**When it'd ship.** After Phase 8 (CLI + admin + migration system) ships, as a standalone ergonomics improvement. No external dependency forces its scheduling; it waits until there's demonstrated demand from users hitting the limits of `raw_query`'s type-inference.

---

## 3. Scope expansions to existing phases (captured 2026-04-20)

During Phase 5-Zero planning the framework's long-term positioning was stress-tested. The gaps below were named; `docs/spec/implementation-plan.md` has been amended accordingly. Listed here so the planning history stays discoverable.

### 3.1 Online / zero-downtime migrations → Phase 6

`docs/spec/implementation-plan.md` §6f now covers advisory-lock-based single-active-migration coordination, lock-timeout on DDL, two-phase column rename, two-phase type widening, safe NOT NULL addition, `NOT VALID` + `VALIDATE` constraint addition, chunked backfill orchestration, and destructive-op gating. Goal: Djogi-generated migrations are safe against live production traffic by default, not just "works on a fresh dev DB."

### 3.2 Observability hooks → Phase 9

`docs/spec/implementation-plan.md` §9 was expanded from a three-line "audit trail + event logging" entry into five concrete sub-sections: tracing spans per query (9b), slow-query callback registration (9c), `metrics` crate integration with per-model breakdown (9d), admin-UI observability surfaces that consume those hooks (9e), and event logging (9f). The audit-trail primitives (9a) are unchanged.

### 3.3 Operational tooling → new Phase 9.5

`docs/spec/implementation-plan.md` now has a Phase 9.5 "Operational Tooling" covering scheduled backups, point-in-time recovery setup, per-model autovacuum tuning, on-demand vacuum, `cargo djogi ops doctor` health checks, and operator runbooks. Goal: turnkey operational story so teams don't hand-roll pg_dump cron jobs inconsistently.

### 3.4 Streaming / cursor terminals → Phase 5 (§5h)

Added to Phase 5 scope: `QuerySet::stream()` returning an `impl Stream<Item = Result<T>>` backed by a Postgres named cursor, pinned to the active `atomic()` scope. Required for analytical workloads that don't fit in memory.

### 3.5 Full-text search → Phase 5 (§5i)

Added to Phase 5 scope: `TsVector` / `TsQuery` types, `#[model(fts = { ... })]` generating `GENERATED ALWAYS AS` columns + GIN indexes, `@@` match predicates on QuerySet filter closures, `ts_rank` ordering helpers. A Postgres-first framework without first-class FTS is incomplete.

### 3.6 Query debugging + analysis → Phase 8 (§8c) and Phase 9 (§9e)

The inefficient-query debugging story is split across two phases because the two halves solve genuinely different problems:

- **Static analysis — Phase 8 §8c.** `cargo djogi analyze query` walks the workspace AST (via `syn`) for call-site discovery and consumes `target/djogi_models.json` (already emitted by `#[model]` + `build.rs`) for FK topology. It ships in two tiers. **Tier 1 (mainline, CI-gating-ready):** loop-shape N+1 detection, `.fetch()` vs `.prefetch()` misuse, and over-fetching detection. Conservative, silent when it cannot resolve the receiver cleanly. **Tier 2 (experimental, warn-only):** graph-aware repeat-node detection — within a scope, track `(model, filter_fingerprint)` pairs reached by terminals and flag redundant re-fetches across unrelated call sites or overlapping prefetch chains. Tier 2 is honest about its ceiling: `syn` alone cannot fully resolve receiver types through generic wrappers, re-exports, or helper indirection; coverage is "high-signal when receiver is unambiguous; silent otherwise". A future upgrade path to rustc/HIR or `rust-analyzer`-as-a-library is named as a follow-up, not a Phase 8c deliverable. Both tiers are pure static — no database connection required — and ship with `--format json` / `--format clippy` for editor and CI integration.
- **Runtime debug drawer — Phase 9 §9e.** Bottom-drawer panel on `/_admin/` pages (and optionally every HTML response in dev mode, via a web-framework sub-feature flag) showing queries issued per request, durations, originating `tracing` spans, and SQL text with inlined binds. Click-to-EXPLAIN defaults to plain `EXPLAIN` (no `ANALYZE`) for zero-side-effect inspection; `EXPLAIN ANALYZE` is an explicit opt-in available for SELECTs only and disabled for mutating statements with a visible note about non-transactional side effects (`nextval` advancement, `LISTEN/NOTIFY`, deferred triggers) not being reclaimed by savepoint rollback. Semantic N+1 annotation driven by declared FK structure rather than pattern matching. API-only apps get per-request correlation via a stable server-generated request ID carried in an `X-Djogi-Queries` header (`id=<token>; count=...; slow=...; total_ms=...`), with full per-query detail retrievable from a dev-mode `GET /_djogi/debug/request/<id>` endpoint backed by a bounded in-memory ring buffer. This avoids the connection-state trap — keep-alive, HTTP/2 multiplexing, client pooling, and multi-instance deployments all make "most recent on this connection" ambiguous, so correlation is explicit. The drawer is dev-only — feature-flagged out of release builds with no staging/canary mode.

Layer 3 (production observability — distributed traces, slow-query callbacks, `metrics` histograms) lives in Phase 9 §9b/9c/9d. Together the three layers form the complete debug story: Phase 8c catches problems before request time in CI, Phase 9e catches them in the local dev loop, Phase 9b/9c/9d surface them in staging + production. There is no hybrid "drawer in staging" surface — non-dev environments use §9b/9c/9d only.

### 3.7 Projection query surface + FK / M2M boundary enforcement → Phase 5 (§5j)

Added 2026-04-21 during a brainstorming session. Phase 4.5 shipped projections as output-shape types only; Phase 5 §5j makes them first-class query entities: `PublicRegisteredOwner::filter(...)` returns `Vec<PublicRegisteredOwner>` with SELECT narrowed to the projection's exposed columns. Compile-time enforcement via generated per-projection `{Projection}Fields` accessor types — out-of-scope field access (`o.address.street` when `AddressPublic` exposes only `.city`) does not compile. Forward-FK, reverse-FK, and M2M (including through-model projection attribution) all participate in the same boundary. Fills the Phase 4.5 explicit deferral "M2M projections — projections nest only through `ForeignKey<T>` / `OneToOneField<T>`; M2M stitching is manual."

### 3.8 `djqry` SQL override registry → Phase 8 (§8d)

Added 2026-04-21 during the same brainstorming session. `djqry/` directory holds hand-tuned SQL files with frontmatter declaring owner(s), return type, binds, and the canonical macro-query the override optimizes. Build pipeline emits typed methods on each listed owner's `djqry` namespace (`Vehicle::djqry::expired_registrations(&mut ctx).await?`), preserving the declarative call-site shape elsewhere while routing the expensive query through hand-written SQL. Build-time fingerprint drift check mandatory; opt-in `cargo djqry verify` runs both paths against a live database and diffs results (CI gate). Every override dispatch fires a tracing event so observability surfaces distinguish hand-tuned queries from macro-generated ones. Read-only in v1; mutations deferred. `@on:` accepts projection types too, composing with §5j.

---

## 4. Unscheduled roadmap items

Items that don't yet have a phase home and don't need one immediately. Captured so they aren't lost.

### 4.1 Postgres version floor — DECIDED 2026-04-20: Postgres 18

Floor locked at Postgres 18. No support for older versions. Rationale (user-stated): Djogi is pre-publish and unapologetic about adoption shape; teams migrating an existing app to Djogi have substantial app-side work regardless, so bundling a Postgres upgrade is a small marginal cost. The framework may freely use any Postgres 18+ feature without version-gating fallbacks. Decision captured in `docs/spec/decisions.md` (Postgres version floor row).

### 4.2 Multi-tenancy beyond `TenantScoped<T>`

`docs/roadmap/security.md` names `TenantScoped<T>` as a planned primitive. Real SaaS deployments use one of three patterns: shared-schema-with-tenant-id (row-level), schema-per-tenant (DDL-per-tenant), database-per-tenant (pool-per-tenant). `TenantScoped<T>` implicitly targets the first. The other two need distinct descriptor and connection-routing primitives — arguably a Phase 10 extension since they interact with topology. Leaving unscheduled pending concrete user demand.

### 4.3 Comparative benchmarks

Djogi should eventually publish criterion-based benchmarks on representative Postgres workloads (single-row CRUD, 1K-row prefetch, 10K-row bulk insert, analytical query with multi-level joins). Numbers establish performance claims; without them, positioning is asserted rather than demonstrated. No phase owns this; it's a quality-discipline line-item for post-Phase-8 when the framework is stable enough to benchmark meaningfully.

### 4.4 Performance SLA / macro overhead articulation

Related to 4.3 but narrower: how much overhead does `#[model]`-generated code add over hand-written `tokio_postgres::Client::query`? The answer is probably "negligible in release builds" but needs measurement. Guide to add when the framework is stable: `docs/guide/performance.md` with headline numbers, criterion benchmarks in-repo, and explicit statement of what "fast" means.

---
