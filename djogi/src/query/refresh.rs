//! Delta-sync fetcher for `QuerySet::refresh_into` — Cluster 8δ T8.3 skeleton,
//! T8.5 SQL implementation, T8.8 always-on LRU eviction warn.
//!
//! # What
//!
//! `DjogiDeltaFetcher<T>` owns a snapshot of the substrate needed to issue
//! delta queries against the source-of-truth Postgres pool: a `DjogiPool`
//! clone, an `AuthContext` by value, and a `BasicPredicate<T>` filter
//! (optional). Each tick of `Punnu::start_delta_refresh(...)` calls
//! `DeltaPunnuFetcher::fetch_delta` on this struct.
//!
//! # Why owned substrate
//!
//! Sassi's `DeltaPunnuFetcher<T>` is `Send + Sync + 'static`. The fetcher
//! lives across ticks, threads, and beyond any single `DjogiContext`'s
//! lifetime. Holding `&mut DjogiContext` or any borrowed substrate would
//! defeat the bound. Each tick reconstructs a fresh `DjogiContext` from a
//! freshly-acquired pool connection + a clone of the captured AuthContext.
//!
//! # Send + Sync auto-derivation
//!
//! No manual `unsafe impl Send` or `unsafe impl Sync` was required.
//! `DjogiPool` and `AuthContext` are `Send + Sync + 'static` outright;
//! `Option<BasicPredicate<T>>` is `Send + Sync` when `T: Send + Sync` (sassi
//! upholds this). `PhantomData<T>` participates in auto-trait inference and
//! is `Send + Sync` exactly when `T: Send + Sync` — that bound is already
//! required by the `DeltaPunnuFetcher` trait impl below, so the inference
//! holds for every well-formed `DjogiDeltaFetcher<T>`. Verified: compilation
//! succeeds without manual impls. The const-fn-pointer assertion at the
//! bottom of this file pins the contract at the type-system level.
//!
//! # T8.5 SQL path
//!
//! `fetch_delta` now issues real SQL on every tick:
//! 1. Acquire a connection from the captured pool.
//! 2. Construct a fresh `DjogiContext::from_connection(conn)` and apply the
//!    captured `AuthContext` via `.with_auth(...)` — auth-locked-to-
//!    subscription per spec §677.
//! 3. Build SQL: `SELECT <COLUMN_LIST> FROM <table_name> WHERE
//!    [<watermark_col> >= $1] [OR id IN ($2, …)] ORDER BY <watermark_col>`.
//! 4. Execute via `ctx.raw_query::<T>(sql, &binds).await`.
//! 5. Split items into `(live_items, tombstones)` via the per-row
//!    `Model::__delta_should_tombstone()` check (T8.6 — Pattern 1,
//!    SoftDeletable-derived); return `DeltaResult::new(live_items, tombstones)`.
//! 6. Drop ctx (releases connection back to pool on drop).
//!
//! # Tombstone collection patterns (cluster 8δ T8.6 → T8.8)
//!
//! - **T8.6 — Pattern 1 (SoftDeletable-derived):** shipped. Per-row
//!   `__delta_should_tombstone()` walks soft-deleted rows into the
//!   tombstones set. Anti-regression: NO `deleted_at IS NULL` filter
//!   in the WHERE clause (deletion signal must flow through the
//!   watermark per spec §415).
//! - **T8.7 — Pattern 2 (outbox-derived):** shipped. Per-tick poll of
//!   `<table>_outbox` for `action='delete'` rows whose `created_at`
//!   advances past a per-fetcher watermark; gated on
//!   `T::descriptor().has_outbox && TypeId::<T::Id> == TypeId::<HeerId>`
//!   (the outbox stores `row_id BIGINT`, so RanjId-keyed models cannot
//!   decode — and `events` is already broken for them at `emit_event`'s
//!   INSERT). Decoded `HeerId` values cast into `T::Id` via
//!   `cast_heerid_to_t_id` (TypeId-checked `transmute_copy`). Closes
//!   GH #128. The poll runs inside the same `transaction::atomic` as
//!   the data SELECT so it inherits the `auto_set_tenant` scope, and
//!   the worker (which uses state transitions, never `DELETE`) cannot
//!   race the poll because outbox rows persist post-publish.
//! - **T8.8 (this commit) — LRU eviction warn (spec §674 Knob 1):** always-on
//!   one-shot warn per `(Punnu, Subscription)` on the first observed
//!   `LruEvict` event. Implemented via `try_recv` per tick + `AtomicBool`
//!   one-shot flag (Option B). See "LRU eviction warn" section below.
//!
//! # T8.8 — LRU eviction warn (spec §674 Knob 1)
//!
//! The fetcher holds two additional fields to support the always-on LRU
//! eviction warn:
//!
//! - `events_rx`: a `tokio::sync::broadcast::Receiver<PunnuEvent<T>>` captured
//!   from `punnu.events()` at `refresh_into` time. Each fetcher instance has
//!   its own independent receiver — per the `(Punnu, Subscription)` scope in
//!   spec §674.
//!
//! - `lru_warn_issued`: an `AtomicBool` one-shot flag. Set on the first
//!   observed `EventReason::LruEvict` event; never cleared across the
//!   fetcher's lifetime.
//!
//! At the top of every `fetch_delta` call, the fetcher drains its events
//! receiver non-blockingly via `try_recv`. If it observes an `LruEvict`
//! event and the flag is not yet set, it emits a single `tracing::warn!` on
//! the `djogi::cache` target and sets the flag. Subsequent ticks skip the
//! warn even if more LRU evictions occur.
//!
//! The `try_recv` call is wrapped in a `Mutex::try_lock` (non-blocking) so
//! concurrent ticks (if sassi ever dispatches overlapping ticks) cannot block
//! on each other — the losing tick simply skips the warn check and yields to
//! the next tick.
//!
//! **Why Option B (try_recv in tick body) over Option A (spawn task)?**
//! Option B avoids a separate spawned task and the lifetime management that
//! comes with it (cancellation signalling, zombie prevention). The overhead is
//! dominated by the SQL round-trip; the `try_recv` loop adds negligible cost.
//! Option A would be cleaner if the warn needed sub-tick latency (e.g.,
//! alerting within milliseconds of eviction), but the production-stability
//! contract (spec §674) only requires "the warn fires before the next tick
//! completes" — Option B satisfies that.
//!
//! # T8.8 — Knobs 2 + 3 (recovery + periodic full refresh)
//!
//! Both `with_eviction_recovery(bool)` and
//! `with_periodic_full_refresh(Option<NonZeroUsize>)` are sassi-native builder
//! knobs on `DeltaRefreshHandle<T>`. Djogi's `refresh_into` returns
//! `sassi::DeltaRefreshHandle<T>` directly, so adopters can chain them:
//!
//! ```text
//! let handle = MyModel::objects()
//!     .refresh_into(&punnu, pool, auth)
//!     .with_eviction_recovery(true)
//!     .with_periodic_full_refresh(NonZeroUsize::new(10));
//! ```
//!
//! No djogi-side wrappers are needed. The methods live on sassi's
//! `DeltaRefreshHandle<T>` and are stable with the exact signatures
//! verified in T8.8 (see `djogi/tests/integration/phase8_t8_8_refresh_knobs.rs`).
//!
//! # Filter pushdown deferral (GH #127)
//!
//! The `self.filter: Option<BasicPredicate<T>>` field is KEPT but not
//! pushed down to SQL in this commit, for two reasons:
//!
//! 1. Sassi's `BasicPredicate<T>` does not expose a `to_sql` method —
//!    verified by grepping `sassi-reference/sassi/src/predicate/`. Writing a
//!    walker over `FieldPredicate<T>` (which carries type-erased values) is a
//!    substantial sub-project, not a T8.5-sized change.
//!
//! 2. GH #126 (filter-api-q-preservation) blocks the practical reach. Until
//!    #126 lands, every real-world `QuerySet`'s `into_basic_predicate()`
//!    returns `None`, so `self.filter` is always `None` in practice. Emitting
//!    SQL for filter pushdown today would be dead code.
//!
//! When `self.filter.is_some()` a `tracing::warn!` fires per tick to surface
//! the gap. In practice this warn never fires today (filter always `None`).

use crate::__bypass::RawAccessExt as _;
use crate::auth::AuthContext;
use crate::cache::DjogiDeltaSyncMeta;
use crate::pg::decode::FromPgRow;
use crate::pg::pool::DjogiPool;
use heeranjid::HeerId;
use sassi::{BasicPredicate, DeltaPunnuFetcher, DeltaQuery, DeltaResult, FetchError, PunnuEvent};
use std::any::TypeId;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_postgres::types::ToSql;

/// Convert a `HeerId` into `T::Id` when `T::Id == HeerId`, else `None`.
///
/// Used by the T8.7 outbox-tombstone path: the outbox table stores
/// `row_id BIGINT`, which decodes to `HeerId` via `HeerId::from_i64`. The
/// fetcher's tombstone set is `HashSet<T::Id>`, so a runtime-checked
/// conversion is needed when `T::Id` is the model's PK type. Today, only
/// HeerId-keyed models route through `#[model(events)]` correctly anyway —
/// the outbox `row_id` column is BIGINT, so RanjId / Serial-keyed models
/// with `events` would already fail at `emit_event`'s INSERT. This helper
/// matches that constraint by no-op'ing when `T::Id != HeerId`.
///
/// `TypeId` equality is the runtime witness that `T::Id` and `HeerId` are
/// the same type. Once that holds, `transmute_copy` is sound — both sides
/// have identical layout (`HeerId` is `#[repr(transparent)]` over `i64`,
/// and `transmute_copy` does a bitwise copy). `HeerId: Copy`, so no
/// double-drop concern. Pattern mirrors `Box<dyn Any>::downcast`'s
/// internal logic without the heap allocation.
fn cast_heerid_to_t_id<TId: 'static>(h: HeerId) -> Option<TId> {
    if TypeId::of::<TId>() == TypeId::of::<HeerId>() {
        // SAFETY: TypeId equality guarantees `TId == HeerId`. Both are
        // `#[repr(transparent)]` over `i64` with identical layout.
        // `HeerId: Copy`, so the source value is trivially copyable.
        Some(unsafe { std::mem::transmute_copy::<HeerId, TId>(&h) })
    } else {
        None
    }
}

/// Owned-substrate fetcher for the `QuerySet::refresh_into` path.
///
/// Holds a clone of the connection pool, an `AuthContext` by value, an
/// optional `BasicPredicate<T>` filter, and the two fields added in T8.8
/// for the always-on LRU eviction warn (spec §674 Knob 1).
///
/// # Send + Sync
///
/// Auto-derived: every field is `Send + Sync` when `T: Send + Sync`.
/// `DjogiPool`, `AuthContext`, `AtomicBool`, and `std::sync::Mutex<_>` are
/// all `Send + Sync`. `broadcast::Receiver<PunnuEvent<T>>` is `Send + Sync`
/// when `PunnuEvent<T>: Send + Sync`, which holds when `T: Send + Sync`
/// (sassi upholds this). `PhantomData<T>` participates in auto-trait
/// inference and is `Send + Sync` exactly when `T: Send + Sync` — that bound
/// is already required by the `DeltaPunnuFetcher` trait impl below. Verified:
/// compilation succeeds without manual impls. The const-fn-pointer assertion
/// at the bottom of this file pins the contract at the type-system level.
pub(crate) struct DjogiDeltaFetcher<T: sassi::DeltaSyncCacheable> {
    pub(crate) pool: DjogiPool,
    pub(crate) auth: AuthContext,
    pub(crate) filter: Option<BasicPredicate<T>>,
    /// One-shot flag — per-(Punnu, Subscription) — for the always-on
    /// LRU eviction warn (spec §674 Knob 1). Set on first `LruEvict`
    /// observation; never cleared across the fetcher's lifetime.
    pub(crate) lru_warn_issued: AtomicBool,
    /// Broadcast receiver from `Punnu::events()` for monitoring LRU
    /// eviction events. Wrapped in `std::sync::Mutex` because
    /// `broadcast::Receiver::try_recv` takes `&mut self` while
    /// `fetch_delta` receives `&self` on the fetcher. `std::sync::Mutex`
    /// (not `tokio::sync::Mutex`) is correct here because `try_recv` is
    /// synchronous — no `.await` is needed to drain the channel.
    pub(crate) events_rx: Mutex<broadcast::Receiver<PunnuEvent<T>>>,
    /// Per-fetcher watermark for the T8.7 outbox-tombstone poll
    /// (Pattern 2). Tracks the highest `created_at` already observed in
    /// `<table>_outbox` so subsequent ticks only see new delete events.
    /// `None` on first tick (poll uses `>= now()` minus a small lookback;
    /// see `fetch_delta` for the boundary policy). `std::sync::Mutex` is
    /// correct because the per-tick update is synchronous (no `.await`
    /// while holding the lock).
    pub(crate) outbox_watermark: Mutex<Option<OffsetDateTime>>,
    pub(crate) _model: PhantomData<T>,
}

#[async_trait::async_trait]
impl<T> DeltaPunnuFetcher<T> for DjogiDeltaFetcher<T>
where
    T: sassi::DeltaSyncCacheable
        + FromPgRow
        + crate::model::Model
        + DjogiDeltaSyncMeta
        + Send
        + Sync
        + 'static,
    T::Watermark: ToSql + Sync,
    T::Id: ToSql + Sync,
{
    async fn fetch_delta(
        &self,
        query: DeltaQuery<T>,
    ) -> Result<DeltaResult<T, T::Watermark>, FetchError> {
        // ── Always-on LRU eviction warn (spec §674 Knob 1) ──────────────────
        // Per-(Punnu, Subscription) one-shot warn. Fires once per fetcher
        // lifetime on the first observed `EventReason::LruEvict` event.
        //
        // Implementation choice: Option B (try_recv in tick body + AtomicBool
        // one-shot flag). This avoids a separate spawned task and the lifetime
        // management (cancellation signalling, zombie prevention) that Option A
        // would require. The per-tick overhead is dominated by the SQL
        // round-trip; the try_recv loop adds negligible cost.
        //
        // `try_lock` ensures we don't block if a concurrent tick (if sassi
        // ever dispatches overlapping ticks) holds the lock — the losing tick
        // skips the warn check this round and yields to the next tick.
        //
        // Two-tier guard. Outer `load(Acquire)` short-circuits the entire
        // drain block once the warn has fired — no Mutex lock acquisition on
        // any subsequent tick. The nested `if let` only attempts `try_lock`
        // when the flag is still false, so the comment's "no lock after warn"
        // claim is a real runtime contract, not a best-effort observation.
        // (The inner `swap(true, AcqRel)` at line 226 is the actual one-shot
        // gate against concurrent races between two ticks both seeing
        // `flag == false` — `swap` returns the old value, so the second
        // racer sees `true` and skips the warn.)
        if !self.lru_warn_issued.load(Ordering::Acquire)
            && let Ok(mut rx) = self.events_rx.try_lock()
        {
            'drain: loop {
                match rx.try_recv() {
                    Ok(PunnuEvent::Invalidate {
                        reason: sassi::EventReason::LruEvict { .. },
                        ..
                    }) => {
                        if !self.lru_warn_issued.swap(true, Ordering::AcqRel) {
                            tracing::warn!(
                                target: "djogi::cache",
                                model = std::any::type_name::<T>(),
                                "Punnu LRU eviction detected — `lru_size` may be \
                                 undersized for this subscription's working set. \
                                 Tune via `PunnuConfig::lru_size` if eviction \
                                 collisions become frequent.",
                            );
                        }
                        break 'drain; // one-shot — no need to drain further
                    }
                    Ok(_) => continue 'drain, // other events — keep draining
                    Err(broadcast::error::TryRecvError::Empty) => break 'drain,
                    Err(broadcast::error::TryRecvError::Closed) => break 'drain,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        // Receiver fell behind — an LruEvict event may have
                        // been dropped. Fire the warn defensively: if the
                        // channel lagged, the Punnu was under heavy eviction
                        // pressure, which is exactly what the warn is meant
                        // to surface.
                        if !self.lru_warn_issued.swap(true, Ordering::AcqRel) {
                            tracing::warn!(
                                target: "djogi::cache",
                                model = std::any::type_name::<T>(),
                                "Punnu event stream lagged — LRU eviction events \
                                 may have been dropped. `lru_size` may be \
                                 undersized for this subscription's working set. \
                                 Tune via `PunnuConfig::lru_size` if eviction \
                                 collisions become frequent.",
                            );
                        }
                        break 'drain;
                    }
                }
            }
        }

        // ── Filter-pushdown gap warning ──────────────────────────────────────
        // Fires per-tick if filter is Some. In practice this never fires today
        // because GH #126 (filter-api-q-preservation) means every real-world
        // QuerySet's into_basic_predicate() returns None. Kept so future
        // BasicPredicate SQL emitters can simply remove this warn block when
        // they land. Tracked at GH #127.
        if self.filter.is_some() {
            tracing::warn!(
                target: "djogi::cache",
                model = std::any::type_name::<T>(),
                "filter pushdown to delta-fetcher SQL emitter is not yet implemented; \
                 refresh tick will fetch the full source-of-truth set within the \
                 watermark window. Tracked at GH #127.",
            );
        }

        // ── Capture per-tick state ───────────────────────────────────────────
        // Auth is locked to the subscription per spec §677: the snapshot
        // captured at refresh_into time is applied to a fresh ctx below.
        //
        // AuthContext::clone() is cheap in practice (small Vec<String>/HashMap).
        // If future profiling shows clone-per-tick is a bottleneck, switching
        // to `Arc<AuthContext>` in the fetcher field is a drop-in optimization.
        let auth = self.auth.clone();
        let since = query.since.clone();
        let recover_ids = query.recover_ids.clone();
        let watermark_col = <T as DjogiDeltaSyncMeta>::WATERMARK_COLUMN;
        let table_name = <T as crate::model::Model>::table_name();
        let column_list = <T as FromPgRow>::COLUMN_LIST;

        // ── T8.7 Pattern 2 outbox-tombstone gate (per-tick) ──────────────────
        // Determine if this model has an outbox table emitted by
        // `#[model(events)]` AND that `T::Id` decodes from the outbox's
        // BIGINT `row_id` column. Today the outbox stores `row_id BIGINT`
        // unconditionally (see Phase 4 `notifications_outbox.sql` schema), so
        // only HeerId-keyed models can decode. Non-HeerId-keyed models with
        // `events` would already fail at `emit_event`'s INSERT — we mirror
        // that constraint here by no-op'ing the outbox poll path. Captured
        // pre-closure so the closure-side check is a cheap copy.
        let outbox_enabled =
            T::descriptor().has_outbox && TypeId::of::<T::Id>() == TypeId::of::<HeerId>();
        let outbox_watermark_snapshot: Option<OffsetDateTime> = if outbox_enabled {
            *self
                .outbox_watermark
                .lock()
                .expect("outbox_watermark mutex poisoned")
        } else {
            None
        };

        // ── Run the SQL inside a transaction so SET LOCAL has effect ─────────
        // `crate::transaction::atomic` issues BEGIN / COMMIT (or ROLLBACK on
        // error) and exposes a `&mut DjogiContext` to the closure. This is
        // load-bearing for spec §677: tenant scope (`SET LOCAL app.tenant_id`)
        // only persists inside an open transaction, and `auto_set_tenant::<T>`
        // is what wires the captured auth's `tenant_id` into Postgres for the
        // subsequent SELECT. Without the transaction wrap + auto_set_tenant,
        // RLS-backed tenant isolation would silently fail (Codex caught this
        // gap in T8.5 round-1 review — orchestrator-fixed in this commit).
        //
        // T8.7 piggy-backs on the same atomic so the outbox poll inherits
        // the tenant scope (RLS-backed `<table>_outbox` policies, when
        // present, see the same `app.tenant_id` setting as the data SELECT).
        // Returns `(items, outbox_tombstones)` — the second slot is empty
        // when `outbox_enabled` is false.
        let (items, outbox_tombstones): (Vec<T>, Vec<(HeerId, OffsetDateTime)>) =
            crate::transaction::atomic(&self.pool, move |ctx| {
            Box::pin(async move {
                // Apply the captured auth snapshot to the inner ctx.
                ctx.set_auth(auth);

                // Apply tenant scope (`SET LOCAL app.tenant_id = '...'`) for
                // tenant-keyed models. No-op for models without `tenant_key`
                // in their descriptor.
                crate::query::terminal::auto_set_tenant::<T>(ctx).await?;

                // Build SQL — 4 explicit cases on (since, recover_ids).
                // Watermark uses `>=` (inclusive boundary per the
                // DeltaPunnuFetcher contract — boundary rows may have changed
                // without their watermark advancing; sassi deduplicates by id).
                // Recovery ids are OR-combined with the watermark clause:
                // we want those rows regardless of watermark progression.
                let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();

                let push_watermark =
                    |params: &mut Vec<Box<dyn ToSql + Sync + Send>>, s: &T::Watermark| -> String {
                        params.push(Box::new(s.clone()));
                        format!("{watermark_col} >= ${}", params.len())
                    };
                let push_recover = |params: &mut Vec<Box<dyn ToSql + Sync + Send>>| -> String {
                    let mut placeholders: Vec<String> = Vec::new();
                    for id in &recover_ids {
                        params.push(Box::new(id.clone()));
                        placeholders.push(format!("${}", params.len()));
                    }
                    format!("id IN ({})", placeholders.join(", "))
                };

                let where_sql: String = match (since.as_ref(), recover_ids.is_empty()) {
                    (None, true) => String::new(),
                    (Some(s), true) => format!("WHERE {}", push_watermark(&mut params, s)),
                    (None, false) => format!("WHERE {}", push_recover(&mut params)),
                    (Some(s), false) => {
                        let watermark_clause = push_watermark(&mut params, s);
                        let recover_clause = push_recover(&mut params);
                        format!("WHERE ({watermark_clause}) OR ({recover_clause})")
                    }
                };

                let sql = format!(
                    "SELECT {column_list} FROM {table_name} {where_sql} ORDER BY {watermark_col}"
                );

                let params_refs: Vec<&(dyn ToSql + Sync)> = params
                    .iter()
                    .map(|b| b.as_ref() as &(dyn ToSql + Sync))
                    .collect();

                let items: Vec<T> = ctx.raw_query::<T>(&sql, &params_refs).await?;

                // ── T8.7 Pattern 2 outbox poll ───────────────────────────
                // Inside the same transaction so the tenant scope set by
                // `auto_set_tenant` above also gates the outbox SELECT.
                // Skip entirely when `outbox_enabled` is false (model has
                // no outbox, or `T::Id != HeerId`). Returns
                // `Vec<(HeerId, OffsetDateTime)>` — the caller decodes
                // HeerId → T::Id and updates the per-fetcher watermark.
                //
                // Boundary semantics: `created_at >= $1` is inclusive.
                // sassi's `apply_delta` deduplicates by id, so re-seeing
                // a tombstone whose `created_at` equals the previous
                // tick's max is harmless — preferable to `>` which would
                // drop legitimate tombstones with sub-microsecond
                // timestamp ties.
                //
                // First-tick semantics: `outbox_watermark_snapshot ==
                // None` ⇒ skip (don't replay history). The fetcher's
                // role is to surface NEW deletes since the cache became
                // visible to the subscription; pre-existing deletes
                // belong to the Punnu's initial-load path, not the
                // delta-tick path.
                let outbox_tombstones: Vec<(HeerId, OffsetDateTime)> = if outbox_enabled
                    && let Some(watermark) = outbox_watermark_snapshot
                {
                    // Validate the outbox table identifier before
                    // embedding into SQL. `table_name` is captured from
                    // `T::table_name()` which is a `&'static str`
                    // emitted by the `#[derive(Model)]` macro and
                    // already validated against the unquoted-identifier
                    // contract at macro-expansion time. The
                    // `_outbox` suffix is a fixed ASCII literal. We
                    // double-check via `check_plain_ident` to keep this
                    // SQL-construction path defense-in-depth aligned
                    // with `outbox/worker.rs::validate_table_ident`.
                    let outbox_table = format!("{table_name}_outbox");
                    crate::ident::check_plain_ident(&outbox_table, false).map_err(|e| {
                        crate::DjogiError::Db(crate::error::DbError::other(format!(
                            "T8.7 outbox poll: invalid outbox table name {outbox_table:?}: {e:?}"
                        )))
                    })?;

                    let outbox_sql = format!(
                        "SELECT row_id, created_at FROM {outbox_table} \
                         WHERE action = 'delete' AND created_at >= $1 \
                         ORDER BY created_at"
                    );
                    let rows = ctx
                        .query_all(&outbox_sql, &[&watermark as &(dyn ToSql + Sync)])
                        .await?;
                    let mut decoded: Vec<(HeerId, OffsetDateTime)> =
                        Vec::with_capacity(rows.len());
                    for row in rows {
                        let raw: i64 = row.try_get(0).map_err(|e| {
                            crate::DjogiError::Db(crate::error::DbError::other(format!(
                                "T8.7 outbox poll: decode row_id i64: {e}"
                            )))
                        })?;
                        let ts: OffsetDateTime = row.try_get(1).map_err(|e| {
                            crate::DjogiError::Db(crate::error::DbError::other(format!(
                                "T8.7 outbox poll: decode created_at: {e}"
                            )))
                        })?;
                        let h = HeerId::from_i64(raw).map_err(|e| {
                            crate::DjogiError::Db(crate::error::DbError::other(format!(
                                "T8.7 outbox poll: HeerId::from_i64({raw}): {e:?}"
                            )))
                        })?;
                        decoded.push((h, ts));
                    }
                    decoded
                } else {
                    Vec::new()
                };

                Ok::<_, crate::DjogiError>((items, outbox_tombstones))
            })
        })
        .await
        .map_err(|e| FetchError::Custom(Box::new(e)))?;

        // ── Derive tombstones from soft-deleted items (T8.6 Pattern 1) ─────
        //
        // The fetcher pulls rows including soft-deleted ones — `auto_set_tenant`
        // + watermark filter advance includes deletion timestamps because
        // `updated_at` advances on save (which the soft-delete path uses).
        //
        // Anti-regression (spec §415): we MUST NOT add `deleted_at IS NULL` to
        // the WHERE clause above; the deletion signal MUST flow through the
        // watermark and be derived here. A `deleted_at IS NULL` filter would
        // silently drop the deletion signal at the SQL boundary, preventing
        // tombstone derivation and leaving stale entries in the Punnu
        // indefinitely. The test `deleted_row_is_tombstoned_not_silently_dropped`
        // in `phase8_t8_6_softdelete_tombstones` pins this invariant.
        //
        // For non-soft-deletable models `__delta_should_tombstone()` always
        // returns `false` (the `Model` trait default), so the loop is a
        // no-op classification pass — tombstones stays empty, backward-compat
        // with T8.5 behavior.
        let mut live_items = Vec::with_capacity(items.len());
        let mut tombstones: HashSet<T::Id> = HashSet::new();
        for item in items {
            if item.__delta_should_tombstone() {
                tombstones.insert(<T as sassi::Cacheable>::id(&item));
            } else {
                live_items.push(item);
            }
        }

        // ── T8.7 Pattern 2: merge outbox-derived tombstones ─────────────────
        // The atomic closure returned `Vec<(HeerId, OffsetDateTime)>` only
        // when `outbox_enabled` was true at gate time. The HeerId → T::Id
        // conversion succeeds whenever `T::Id == HeerId` (which the gate
        // already verified), so a `cast_heerid_to_t_id` returning `None`
        // here is a logic bug — surface it via debug-assert rather than
        // silently dropping the tombstone.
        //
        // Watermark advancement: take the max of all observed `created_at`
        // values. Stored back into `self.outbox_watermark` so subsequent
        // ticks only poll for newer events. The advance happens AFTER the
        // tombstones have been merged so any panic during conversion does
        // not leave the watermark advanced past unprocessed events.
        if !outbox_tombstones.is_empty() {
            let mut max_seen: Option<OffsetDateTime> = None;
            for (h, ts) in &outbox_tombstones {
                if let Some(t_id) = cast_heerid_to_t_id::<T::Id>(*h) {
                    tombstones.insert(t_id);
                } else {
                    debug_assert!(
                        false,
                        "T8.7 outbox poll: cast_heerid_to_t_id returned None despite TypeId gate"
                    );
                }
                max_seen = Some(match max_seen {
                    None => *ts,
                    Some(prev) if *ts > prev => *ts,
                    Some(prev) => prev,
                });
            }
            if let Some(new_watermark) = max_seen {
                let mut guard = self
                    .outbox_watermark
                    .lock()
                    .expect("outbox_watermark mutex poisoned");
                // Monotonic advance: only update if new_watermark is
                // strictly greater than the snapshot, in case a concurrent
                // tick (if sassi ever dispatches overlapping ticks) advanced
                // the watermark while this tick was in flight.
                let advance = match *guard {
                    None => true,
                    Some(prev) => new_watermark > prev,
                };
                if advance {
                    *guard = Some(new_watermark);
                }
            }
        } else if outbox_enabled
            && self
                .outbox_watermark
                .lock()
                .expect("outbox_watermark mutex poisoned")
                .is_none()
        {
            // First tick with outbox enabled but no events to poll yet
            // (because watermark snapshot was `None`). Initialise the
            // watermark to the current wall-clock so subsequent ticks
            // start from this checkpoint rather than replaying history.
            // Use `now()` from the database is preferable to host clock
            // (avoids skew between fetcher host and DB host); deferring
            // for simplicity since outbox events carry DB-side
            // `created_at` and the fetcher only compares to its own
            // historical observations.
            *self
                .outbox_watermark
                .lock()
                .expect("outbox_watermark mutex poisoned") = Some(OffsetDateTime::now_utc());
        }

        // `DeltaResult::new` sets `high_watermark = None` — Sassi's
        // `observed_watermark()` will infer the high watermark from
        // `max(item.watermark())` across the returned items. We never emit a
        // synthetic high_watermark past what the query returned, so omitting
        // it is correct (and preserves the invariant that the watermark only
        // advances based on observed rows).
        //
        // Note: watermark inference runs over `live_items` only because
        // djogi never places tombstoned rows in `DeltaResult.items` to begin
        // with — the split happens at this djogi boundary, not inside sassi.
        // Sassi's `observed_watermark()` scans `self.items` with no
        // tombstone awareness; the exclusion is purely a consequence of how
        // we construct the DeltaResult. Practical effect: a tombstoned row
        // with a higher `updated_at` than every live row will NOT advance
        // the next tick's `since` filter — that is the intended behavior
        // (a deletion is not itself a new high-water checkpoint). Sassi's
        // `apply_delta` evicts tombstoned ids from the Punnu and emits
        // `PunnuEvent::Invalidate { reason: EventReason::OnDelete }`.
        //
        // T8.7 / T8.8 hook here: when Pattern 2 (outbox) and Pattern 3
        // (delete-log) tombstones land, they merge into the same `tombstones`
        // HashSet before the `DeltaResult::new(...)` call below.
        Ok(DeltaResult::new(live_items, tombstones))
    }
}

// Compile-time assertion that `DjogiDeltaFetcher<T>: Send + Sync + 'static`
// for any `T: DeltaSyncCacheable + Send + Sync + 'static`. Sassi's
// `start_delta_refresh` requires this bound on the fetcher; auto-derivation
// is mechanically correct today, but a future refactor that adds a
// non-Send/Sync field (e.g. an `Rc<...>` or a borrowed reference) would
// silently break the contract. This const-fn-pointer captures the proof at
// compile time so any such regression fails the build instead of surfacing
// later as an opaque trait-bound error at the `start_delta_refresh` call site.
const _: fn() = || {
    fn _assert_send_sync_static<T: Send + Sync + 'static>() {}
    fn _check_fetcher<T: sassi::DeltaSyncCacheable + Send + Sync + 'static>() {
        _assert_send_sync_static::<DjogiDeltaFetcher<T>>();
    }
};
