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
