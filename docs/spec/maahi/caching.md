> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)

# Maahi — Caching and Cross-Runtime State

Maahi is a hydrated WASM client backed by Dioxus server functions. Latency between operator action and visible response matters; so does cross-tab coherence in multi-operator deployments. Maahi delegates caching, cross-runtime predicate evaluation, and cross-process invalidation to the [sassi](https://github.com/TarunvirBains/sassi) sibling crate — a typed in-memory pool with composable predicate algebra and pluggable cache backends. Sassi ships ahead of Maahi (sassi v0.1.0 alongside djogi v0.1.0; Maahi is djogi v0.3.0), so by Phase 10 every primitive below is on crates.io.

## Why sassi

Maahi's UI patterns map cleanly onto sassi primitives:

| Maahi pattern                       | sassi primitive                                                 |
|-------------------------------------|-----------------------------------------------------------------|
| List view page state                | `Punnu<{Model}Visage>`                                          |
| FK preload tier                     | `Punnu<TargetVisage>` with TTL                                  |
| FK typeahead                        | `Punnu::get_or_fetch` with single-flight coalescing             |
| Filter widget composition           | Djogi `PortablePredicate<T>` lowered to Sassi `BasicPredicate<T>` |
| Per-(role, model, visage) isolation | Doctrine A — type-monomorphic Punnu per visage                  |
| Multi-tab cache coherence           | `sassi-cache-redis` pub/sub fan-out                             |
| Save-on-commit cache invalidation   | Sassi's `on_commit` hook on the existing Phase 4 outbox path    |
| Audit log paging                    | `Punnu<AuditEntry>` with the same visibility filter as live views |

Each integration point below traces back to one of these. Maahi adds zero new caching code; everything below is configuration + dispatch.

## Per-(role, model, visage) Punnu mapping

Maahi's RBAC grants visages per role per model (see [RBAC](./rbac.md) `_admin_role_visage_perms`). At session start, Maahi materialises the operator's effective grant set and instantiates one `Punnu<VisageT>` per granted (model, visage) pair. Sassi's type-monomorphic Punnu doctrine — different Rust types get different Punnu instances by construction — is the structural fit: a role with `view` on `VehiclePublic` and `view + edit` on `VehicleAdmin` holds two distinct Punnu instances, not one with conditional projection.

Switching tenants (per [Architecture — Multi-Tenancy](./architecture.md)) drops the session's Punnu set and rebuilds it for the new tenant context. Reassignment via `manage_users` similarly drops + rebuilds at next login. The Punnus are session-bound; they are never shared across operators or sessions.

## List views

[UI Surface — List Views](./ui.md) describes pagination, sort, filter, and search with URL-encoded canonical state. Maahi inserts a sassi layer between URL state and rendered DOM:

1. URL state changes → server function dispatches a query → results stream back as `Vec<{Model}Visage>`
2. Results land in the session's `Punnu<{Model}Visage>` (`Punnu::insert_many`)
3. Filter / sort recomposition *within the loaded set* runs client-side against the cached Punnu — sub-millisecond response, no round-trip
4. Filter / sort that *crosses the cache boundary* (e.g., a date-range filter that admits rows beyond the current page) triggers a fresh server-function call that re-populates the Punnu

URL state stays authoritative — bookmarks, deep links, and shareable URLs work unchanged. The Punnu is the speed layer, not the source of truth. `admin_page_size` governs the visible window; sassi's `PunnuConfig::lru_size` governs the working set retained for client-side filter recomposition. Default `1000` per (role, model, visage) Punnu, tuned for typical admin browsing depth. Override via `[admin].punnu_list_lru_size` in `Djogi.toml`.

## Delta-sync — incremental refresh

List-view freshness without round-tripping the whole result set on every refresh: sassi's [`DeltaSyncCacheable` and delta-refresh primitives](https://github.com/TarunvirBains/sassi/blob/main/docs/concepts.md#delta-sync-and-watermarks) are the primitives, djogi's `QuerySet::refresh_into` (Phase 8f) is the backend wrapper, and Maahi exposes a Dioxus server function that bridges the wire.

Every djogi `Model` carries `updated_at` as a framework-injected column (CLAUDE.md guarantee, non-negotiable); `#[derive(Model)]` auto-emits the `DeltaSyncCacheable` impl pointing at `updated_at`. Maahi inherits whatever watermark the adopter declared on the Model — override mechanics (alternate field, composite, domain-monotonic, custom newtype) live in Sassi's delta-refresh contract and djogi Phase 8f, not here. The watermark is tracked per-`RefreshSubscription`, not per-Punnu — a single `Punnu<T>` may be the target of multiple subscriptions for different filter shapes, each with its own independent watermark. On each refresh tick:

1. Maahi's WASM client calls the `delta_<model>` Dioxus server function with `(filter_envelope, watermark)`. The filter envelope carries the operator's current Djogi-provenanced portable predicate, serialized via the adopter-facing predicate transport envelope, the same envelope the [Filter algebra — cross-runtime](#filter-algebra--cross-runtime) section uses for predicate transport.
2. Server-side, the function deserializes the predicate, validates it against the operator's RBAC (visage scope check via the existing per-(role, model, visage) gate), and runs the fetch under a freshly-constructed per-request `DjogiContext` (built from a pool connection + the request's `AuthContext`). Initial no-watermark loads apply `WHERE <portable_filter>`. Delta ticks do not re-apply that filter; they fetch by watermark and eviction-recovery id clauses, upsert every changed live row, and return `DeltaResult { items: Vec<{Model}Visage>, tombstones: HashSet<{Model}::Id>, watermark: Watermark }`.
3. WASM client applies the delta via sassi's `Punnu::apply_delta`, atomically committing items + tombstones in one snapshot-swap. Identity-map dedup handles ties at the watermark boundary, and later `filter_basic` calls exclude rows that transitioned out of the current predicate. Tombstones are reserved for true source deletes; tombstone precedence ensures soft-deleted rows are evicted at the same commit. The operator's UI re-renders with the new entries — typically sub-frame because the diff is small.

The server-function pattern is the per-call construction shape from Sassi's fetcher-ownership contract: there is no long-lived `DeltaRefreshHandle<T>` on the server side because each server-function invocation is a single ad-hoc tick, not a subscription.

Bandwidth scales with churn rate (rows changed since last tick), not with cache size. A 10k-row admin browse that refreshes every 30s with 50 row updates per minute pulls ~25 rows per tick instead of the full 10k. The same primitive runs backend-side: a Punnu cached on the server (cross-tab coherence layer) refreshes via the same delta query without going through the server function — `QuerySet::refresh_into` directly hits the database.

### LRU eviction and refresh subscriptions

Punnu is bounded by `max_entries`; an entry tied to a refresh subscription that doesn't get read recently can be LRU-evicted. The delta query then misses it (its `updated_at` is below the watermark, the OR-id-IN clause isn't there by default), and the cache develops a gap relative to the canonical filter result.

Per sassi §3.9.1's three independent knobs, Maahi's `[admin]` block exposes:

- **Always on (no config) — sized-to-fit + warn-on-eviction.** A `tracing::warn!` fires once per `(Punnu, RefreshSubscription)` pair on first eviction collision, telling the operator (or the operator's monitoring) that `punnu_list_lru_size` may be undersized for the working set.
- **`punnu_eviction_recovery = bool`, default `false`** — flips the sassi `with_eviction_recovery(true)` knob. Wires per-subscription event subscriber + recovery query branch. Best-fit shape: high-churn admin browsing where eviction is frequent and gap latency matters.
- **`punnu_periodic_full_refresh_every = Option<usize>`, default `None`** — flips the sassi `with_periodic_full_refresh(Some(n))` knob. Every Nth tick replaces the delta query with a full re-fetch; watermark re-baselined + LRU/schema drift refreshed. Best-fit shape: bandwidth-capped deployments where occasional spikes are acceptable for guaranteed coherence; deletion cleanup still flows through tombstones (soft-delete via Tracked or — for hard-deletes — future cross-runtime push from Phase 11+).

Adopters compose. A typical admin browse over a few-hundred-row working set runs on the default (no config); a deployment browsing a 50k-row table with frequent third-party-edits enables `punnu_eviction_recovery = true`; an audit-log Maahi pulling from a write-heavy log database adds `punnu_periodic_full_refresh_every = 30` to amortise consistency spikes.

### Deletion handling — via tombstones

Watermark delta-sync catches inserts + updates + soft-deletes through the watermark itself. Sassi never infers deletion from absence — the fetcher reports deletions explicitly via `DeltaResult.tombstones` (per sassi §3.9.1 "Deletion handling — tombstones, not absence"). Sassi commits items + tombstones atomically via `Punnu::apply_delta`, where the tombstone-precedence rule evicts soft-deleted rows at commit time, emitting `PunnuEvent::Invalidate { id, reason: EventReason::OnDelete }` per evicted id. Tombstones derive from soft-delete (`Tracked`-fetcher includes deleted rows; `collect_tombstones` extracts PKs where `deleted_at.is_some()`) or outbox subscription (backend-side hard-delete capture).

**Backend-side Punnus** (server-process caches): two patterns adopters compose.

1. Soft-delete via `Tracked` (default for cached models). The fetcher includes soft-deleted rows in `items` so the delta-sync layer derives tombstones via `collect_tombstones`; sassi's `apply_delta` tombstone-precedence rule evicts soft-deleted rows at the commit boundary. The cache state at commit excludes soft-deleted rows. The adopter's QuerySet filter passed to `refresh_into` does NOT include `deleted_at IS NULL` (that would exclude the deletion signal). At render time, the UI's `MemQ::filter` predicate (e.g., `deleted_at.is_null()`) is the defensive equivalent of a visibility filter — useful for code that reads through a partial tick boundary, but the canonical post-commit state never holds soft-deleted rows.
2. Outbox event subscription for hard-deletes (models without `Tracked`, or hard-deletes outside the soft-delete contract). The backend Punnu's fetcher subscribes to djogi's outbox `OnDelete` stream (Phase 4), accumulates IDs locally, drains them as `tombstones` on the next `fetch_delta` call.

**WASM-tier Punnus** (Maahi browser session caches): the outbox is a backend-only concept — there is no in-process channel between djogi's outbox writer and a WASM-runtime Punnu. WASM Punnus rely on:

1. Soft-delete via `Tracked` (default — covers ~95% of cached models). Same pattern as backend: fetcher includes soft-deleted rows in `items`; the delta-sync layer derives tombstones from `deleted_at`; `apply_delta` commits both atomically with tombstone precedence. The Dioxus server function returns `DeltaResult { items, tombstones, watermark }` over the wire envelope; the WASM client applies the delta via `Punnu::apply_delta` with the same precedence rule.
2. `punnu_periodic_full_refresh_every` for re-baselining; combined with (1) covers most cases. Hard-deletes (no soft-delete trail) on WASM-tier Punnus are not caught until cross-runtime push (Phase 11+) is specced.

A future Phase 11 amendment may introduce a server→WASM event push channel (WebSocket / SSE) carrying outbox events; until then, WASM Punnus rely on the soft-delete + tombstone-derivation pattern (server function returns DeltaResult, WASM client applies via apply_delta); hard-deletes outside the soft-delete contract are covered only via cross-runtime push (Phase 11+).

### AuthContext and RLS coherence

The Dioxus server function reads the operator's `AuthContext` from request state at every invocation and constructs a per-request `DjogiContext` to run the delta query. RLS / tenant-binding is automatic — the same auth scope that gated the operator's original list-view query gates every subsequent delta tick because the WASM client's session-scoped state already binds the AuthContext, and Maahi's session-bootstrap pairs the AuthContext with the WASM-side `DeltaRefreshHandle<T>` so subsequent ticks ride the same auth scope. Switching tenants drops the entire session's Punnu set per the [Per-(role, model, visage) Punnu mapping](#per-role-model-visage-punnu-mapping) section, which incidentally cancels every active `DeltaRefreshHandle`. New tenant context starts fresh — Maahi rebuilds the Punnu set + new refresh handles bound to the new AuthContext.

A backgrounded refresh tick that races a tenant-switch never happens — Maahi's WASM client waits for the tenant-switch confirmation before issuing any further server-function calls, and the dropped Punnu set tears down its refresh tasks before the new context is bound.

## FK preload tier

[UI Surface — FK Widget Tiers](./ui.md) defines the preload tier: target tables under `fk_preload_threshold` (default `200`) load all rows at form-render time, materialising options into a static `<select>`. The preload set IS a Punnu — instantiated at form-open, populated by one server-function call, dropped at form-close. TTL configured via `[admin].fk_preload_ttl` (default `5min`) for forms left open across multiple edits.

Multiple FK fields targeting the same table share a single Punnu: sassi's identity-map invariant (one `id()` → one entry) means a single preload populates accessors for every form field that points at the same target. A two-FK-to-User form fires one server-function call, not two.

## FK typeahead

[UI Surface — FK Widget Tiers](./ui.md) defines the typeahead tier: target tables at-or-above the threshold debounce 300ms then dispatch a server function. Maahi routes the dispatch through `Punnu::get_or_fetch` so concurrent typeaheads (multiple FK fields on the same form, multiple keystrokes within the debounce window) collapse via sassi's single-flight registry — one server-function call serves N overlapping intents, not N round-trips. The single-flight contract (sassi spec §3.5.1) covers cancellation cleanly: peers waiting on the same fetch share a result; if every peer drops, the fetch is dropped too.

Batch FK label rendering (e.g., a list view showing 25 rows each with FK references requiring labels) uses `Punnu::get_or_fetch_many`: one server function returns all 25 labels in a single round-trip. The "select all matching" preview before a bulk action benefits from the same primitive.

## Filter algebra — cross-runtime

The headline cross-runtime Sassi win for Maahi. [UI Surface — List Views](./ui.md) describes per-field filter widgets auto-typed from `FieldDescriptor::ty`. Each widget produces a Djogi `PortablePredicate<T>`; composed predicates use the `&`, `|`, `^`, `!` operators. The same portable predicate intent runs in two places:

- **Server-side**, lowering to SQL via djogi's `Q::Portable(PortablePredicate<T>)` walker (Phase 8e), for cache-boundary filter evaluation that crosses pagination
- **Client-side**, lowering the trusted portable predicate to Sassi `BasicPredicate<T>` and evaluating against the cached `Punnu<T>` for instant filter feedback within the loaded set

A single source of truth — the portable predicate value — drives both runtimes. Maahi writes filter logic once and gets server-side and client-side evaluation for free. Djogi owns provenance and SQL lowering; Sassi owns in-memory replay. The operator sees identical filter semantics regardless of which side the evaluation happens on.

[`Q<T>` djogi-only extensions](../queries.md) (raw-pattern `Ilike`, full-text search, JSONB path, spatial predicates) are SQL-only; they only run server-side. Maahi filter widgets that emit those extensions trigger a server round-trip every state change. Structured predicate widgets — `Eq`, `Neq`, `Gt`, `Between`, `In`, `IsNull`, `Contains`, `IContains`, etc. — stay client-side after the initial fetch.

## Multi-tab invalidation

[Operations — Bulk Operations](./operations.md) and [UI Surface — M2M Through-Table Inlines](./ui.md) describe mutations that affect data visible in other operator tabs. Maahi enables `sassi-cache-redis` (a sassi v0.1.0 companion crate) as the L2 backend in production deployments; cross-process invalidation fires through Redis pub/sub.

Operator A in tab 1 saves a `Vehicle`; sassi's `on_commit` hook fires invalidation for the `vehicles` Punnu in operator B's tab 2 within milliseconds. Tab 2 sees the row update on its next render cycle without manual refresh, without polling, and without any Maahi-side coherence code.

Single-tenant single-operator deployments can run on the default `NoBackend` (L1-only, no cross-process layer) — Maahi's `[admin].cache_backend` selector picks `"redis"` or `"none"`.

## Save-on-commit cache coherence

Phase 4's outbox already rides the `on_commit` substrate. Sassi's invalidation hook (Phase 8f, djogi-side) integrates through the same callback registry. Every Maahi save path — single-row `Update`, M2M inline edits, bulk-delete approval execution — wraps in a transaction. On commit, the relevant Punnu instances invalidate the affected ids automatically.

Maahi writes zero cache-invalidation code. The combination of `on_commit` registration + `Cacheable::Id`-keyed invalidation gives transactional cache coherence by construction. On rollback, neither outbox nor Punnu invalidation fires, so cache state stays consistent with the rolled-back transaction.

## Audit log caching

[Operations — Audit Log Access](./operations.md) describes audit log paging with a visibility filter (per-role per-model field redaction) applied at server-side query time. Audit views use the same caching pattern as live list views — `Punnu<AuditEntry>` with the visibility filter applied during the server-function fetch. The audit Punnu is per-(role, model) for the same reason live list Punnus are: different roles see different visible field sets and must not share a cache.

## What sassi does NOT cache

Boundaries — to keep the integration honest:

- **CSRF tokens, session state** — auth substrate; persistent in `_admin_sessions`, never cached
- **Permission decisions** — visage grants resolve fresh at every server-function entry per [RBAC](./rbac.md), never cached
- **Aggregations and charts** — djogi's `group_by` / window functions compute server-side; sassi caches the resulting rows but not the computation
- **Approval workflow state** — `_admin_pending_actions` is durable state, queried directly per [Operations](./operations.md)
- **`_admin_users` / `_admin_sessions`** — auth tables; bypassing the freshness contract on these would let stale auth state survive a password rotation

## Configuration

`[admin]` block keys re-export the sassi `PunnuConfig` knobs Maahi exposes:

```toml
[admin]
# Per-Punnu LRU size for list-view caches (sassi PunnuConfig::lru_size).
# Default: 1000 entries per (role, model, visage) Punnu.
punnu_list_lru_size = 1000

# TTL for FK preload caches (sassi PunnuConfig::default_ttl).
# Default: 5 minutes — long enough for an open form, short enough that
# a stale dropdown doesn't outlive a deployed schema migration.
fk_preload_ttl = "5min"

# Cross-process invalidation backend.
# - "none"  = L1-only; single-instance / single-tab deployments
# - "redis" = sassi-cache-redis pub/sub fan-out; required for any
#             deployment with multiple operator sessions or tabs
# Default: "none".
cache_backend = "none"
cache_redis_url = "redis://localhost:6379"

# sassi PunnuConfig::namespace prefix for Redis keys. Useful when one
# Redis instance serves multiple Maahi deployments or shares with the
# adopter's own sassi usage. Default: "maahi" so Maahi's keys are
# disjoint from any per-app sassi keys at the same Redis instance.
cache_namespace = "maahi"

# sassi PunnuConfig::event_channel_capacity for Punnu event broadcast.
# Default: 256 — lossy by design (sassi spec §3.5); slow subscribers
# get RecvError::Lagged, the Punnu producer never blocks. Tune up if
# Maahi has many simultaneous live subscribers per session.
cache_event_channel_capacity = 256
```

Maahi's hydrated WASM client requires sassi's `runtime-wasm` feature; sassi WASM-target compatibility tracks via [sassi issue #3](https://github.com/TarunvirBains/sassi/issues/3) and lands through sassi's `PunnuExecutor` abstraction. Maahi inherits the WASM-target story automatically — once sassi is wasm-clean, Maahi compiles wasm-clean.

## Phase boundary

Cache substrate is a Phase 10 v1 deliverable; the eight integration points above ship together. Phase 10.5 candidates include:

- Collection-aggregate caches for batched FK label rendering (the `Punnu<BlockedSet>`-style wrapper pattern from sassi spec §3.1.1 Doctrine B)
- Per-session metrics dashboards via `sassi::PunnuMetrics` — operator-facing cache hit rate / eviction / fetch latency surfaced in the admin UI
- Configurable per-Punnu invalidation policies (e.g., shorter TTLs on auth-adjacent models, longer on lookup tables)
- `Sassi::all_impl::<dyn AdminAction>()` cross-type queries when Maahi grows action plugins beyond the v1 six-action model

None of those are v1; v1 ships the integration above and stops there.

---

> [Back to README](../../../README.md) | [All Specs](../index.md) | [Maahi](./index.md)
