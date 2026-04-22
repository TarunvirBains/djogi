# Adoption Readiness

This document maps common app patterns to the earliest Djogi phase at which they are "safe to adopt" — meaning the needed primitives ship, are tested, and have the performance and concurrency semantics required for production app code.

**Sourcing:** this table is maintained as phases merge. Each phase plan owns the patterns it introduces; this doc aggregates them in one place. The initial population covers Phases 1 through 11 from the canonical implementation plan (`docs/spec/implementation-plan.md`) and the planned midpoint phases (4.5, 5.5, 6.5, 7.5, 8.5, 9.5).

**Canonical sequence reference:** `docs/spec/implementation-plan.md`.

---

## Per-Phase Adoption Matrix

| Pattern | Safe at Phase | Notes |
|---------|---------------|-------|
| Basic CRUD on simple models | 1 | `#[derive(Model)]` + `create` / `save` / `delete` / `find` |
| Typed queries (filter / order / limit) | 2 | `QuerySet<T>` + `FieldRef` closures |
| Foreign keys + prefetch / select_related | 3 | `ForeignKey<T>` + M2M through models |
| Transactions + `atomic` / `on_commit` / savepoints | 4 | `DjogiContext` + `atomic()`; foundation for concurrency-safe write paths |
| Row locks + bulk upsert / bulk update | 4 | `select_for_update` + `bulk_upsert`; critical for queue claim flows and high-write systems |
| Transactional outbox (write side) | 4 | `#[model(events)]` + ctx-aware CRUD |
| Idempotent creates | 4 | `#[model(idempotency_key)]` + `create_or_find` |
| Scoped sequence numbers | 4 | `#[field(sequence_within)]` — Phase 4 v3 Task 7.6 |
| Error classification (retry on transient) | 4 | `DjogiError::is_transient` / `is_terminal` — Phase 4 v3 Task 7.7 |
| Projection structs for HTTP boundaries | 4.5 | `#[field(expose)]` + generated DTOs; transport-safe boundary, not query-time aggregation shape |
| Dirty tracking + optimistic locking | 5 | `Tracked<T>` + `#[field(version)]`; write-side correctness under concurrency |
| Bounded string vs text schema primitives | 5 | Distinct `VARCHAR(n)` / `TEXT` modeling so migrations preserve intent instead of flattening both into generic `String` |
| Typed Postgres enums | 5 | `#[derive(DjogiEnum)]` |
| Array fields with native operators | 5 | `contains` / `contained_by` / `overlap` / `len`; keeps native Postgres operators in-framework |
| `Jsonb<T>` with unknown-field preservation | 5 | Both flat `.path::<V>("...")` and typed `#[derive(JsonbSchema)]`; keeps JSON-heavy query paths typed and native |
| Multi-tenancy (RLS + `set_tenant`) | 5 | `#[model(tenant_key)]` + `ctx.set_tenant` |
| `_insecurely()` bypass surface | 5 | Searchable + observable via `tracing::warn!` |
| Outbox worker + publishers | 5 | Phase 5 v3 Task 11.5 — NOTIFY default; Redis / Kafka / NATS feature-gated; exponential backoff on retryable failures |
| Cursor-backed streaming terminals | 5 | `QuerySet::stream` / `DjogiContext::raw_stream` over Postgres named cursors; transaction-scoped |
| Full-text search | 5 | `#[model(fts = { source, dictionary })]` + `TsVector` / `TsQuery`; GIN index emitted for the tsvector column |
| Authentication + session management | 5.5 | `DjogiAuth` + `EnvAuth` + `SessionStore` |
| Password hashing (Argon2) | 5.5 | `PasswordHash` (feature `auth-argon2`) |
| Axum integration | 5.5 | `FromRequestParts` (feature `auth-axum`) |
| Spatial (GeoPoint + ST_DWithin + GIST auto-index) | 6 | PostGIS-backed |
| Production migrations (`djogi migrate`) | 7 | Differ + CLI + snapshot; schema changes stop being hand-written |
| Protected-data metadata + field codecs | 7.5 | `#[field(sensitive, codec)]` |
| Lifecycle hooks + computed properties + composition | 8 | `#[abstract_model]` + `SoftDeletable` / `Auditable` |
| Partition-aware QuerySet | 8 | `#[model(partition_by)]` |
| Shell + admin panel | 9 | Rhai REPL + auto-generated admin |
| Lifecycle governance + data purge / anonymize / archive | 9.5 | `djogi plan` → `show` → `apply` |
| CRUD log + observability + optional Redis cache | 10 | Three-DB architecture + `#[model(cache_ttl)]` |
| Distributed topology + read modes + residency | 11 | Topology metadata + migration guardrails |

---

## Methodology

A pattern is "safe at Phase N" when all four of the following hold:

1. The required primitives ship in Phase N's merged code.
2. The primitives pass Phase N's test suite including integration tests.
3. The canonical guide (`docs/guide/*.md`) documents the pattern with a runnable example.
4. No explicit deferral marker in Phase N's plan points at a later phase as the home.

For performance-sensitive patterns, "safe" also means the phase exposes the efficient Postgres form in-framework. If the only practical way to keep query count, lock behavior, or write throughput acceptable is to fall back to raw SQL for routine cases, the pattern is not yet safe to adopt.

Patterns that straddle phases (e.g., "outbox" — write side lands in Phase 4, worker side lands in Phase 5) are listed at the phase where the full end-to-end flow becomes usable in app code.

The table is intentionally primitive-first, but several large-scale workload families depend on multiple rows together rather than one isolated feature:

- High-volume feed reads depend on typed queries, explicit eager loading, expression power, aggregation, and efficient pagination.
- Concurrent engagement writes depend on transactions, row locks, bulk/upsert primitives, idempotency, and retry classification.
- Queue/job systems depend on row locks, `skip_locked`-style claim flows, chunked reads, and outbox/event plumbing.
- Multi-tenant SaaS depends on RLS, scoped bypasses, and later topology/read-mode phases.

---

## Updating this Doc

**Whenever a phase merges, the merging worker MUST update this table** with:

- New patterns that became safe at that phase.
- Patterns that were tentatively safe at an earlier phase but required fixes that landed in this phase (note the history in the "Notes" column).
- Removal of patterns that were deferred to a later phase during implementation.

This is codified in the `/complete-phase` skill. Skipping the update is a merge blocker — the skill checks this file's mtime against the merged phase's plan mtime and refuses to close out the phase if the spec doc is older.

---

## Cross-References

- `docs/spec/implementation-plan.md` — canonical phase sequence and scope definitions.
- `docs/spec/scope.md` — public vs internal API boundaries (determines what's safe to depend on).
- `docs/spec/architecture-principles.md` — design principles that frame "safe to adopt".
