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
//!    `Model::__delta_should_tombstone()` check (Pattern 1,
//!    SoftDeletable-derived); return `DeltaResult::new(live_items, tombstones)`.
//! 6. Drop ctx (releases connection back to pool on drop).
//!
//! # Tombstone collection patterns
//!
//! - **Pattern 1 (SoftDeletable-derived):** per-row
//!   `__delta_should_tombstone()` walks soft-deleted rows into the
//!   tombstones set. Anti-regression: NO `deleted_at IS NULL` filter
//!   in the WHERE clause — the deletion signal must flow through the
//!   watermark per spec §415.
//! - **Pattern 2 (outbox-derived):** per-tick poll of
//!   `<table>_outbox` for `action='delete'` rows whose `created_at`
//!   advances past a per-fetcher watermark; gated on
//!   `T::descriptor().has_outbox && TypeId::<T::Id> == TypeId::<HeerId>`
//!   (the outbox stores `row_id BIGINT`, so non-HeerId-keyed models
//!   would already have failed at `emit_event`'s INSERT). The poll
//!   runs inside the same `transaction::atomic` as the data SELECT so
//!   it inherits the `auto_set_tenant` scope. Closes GH #128.
//!
//! # LRU eviction warn (spec §674 Knob 1)
//!
//! Always-on, one-shot per `(Punnu, Subscription)`: on the first observed
//! `LruEvict` event, emit one `tracing::warn!` on the `djogi::cache`
//! target. Implemented via `try_recv` per tick + `AtomicBool` flag —
//! drains the receiver inside `Mutex::try_lock` so a losing tick skips
//! the check rather than blocking. Cost is dominated by the SQL
//! round-trip; the drain loop is negligible.
//!
//! # Knobs 2 + 3 (recovery + periodic full refresh)
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
//! No djogi-side wrappers are needed.
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
use heeranjid::{HeerId, HeerIdDesc};
use sassi::{BasicPredicate, DeltaPunnuFetcher, DeltaQuery, DeltaResult, FetchError, PunnuEvent};
use std::any::TypeId;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_postgres::types::ToSql;

/// True when `T::Id` is one of the BIGINT-decoded HeerId flavours that
/// the outbox `row_id BIGINT` column can round-trip through. Both
/// `HeerId` and `HeerIdDesc` are `#[repr(transparent)]` over `i64`, so
/// the stored bits round-trip identically through the column.
fn t_id_decodes_from_outbox_bigint<TId: 'static>() -> bool {
    TypeId::of::<TId>() == TypeId::of::<HeerId>()
        || TypeId::of::<TId>() == TypeId::of::<HeerIdDesc>()
}

/// Convert outbox `row_id` (decoded as `i64`) into `T::Id` when
/// `T::Id` is one of the BIGINT-shaped HeerId flavours. The `i64`
/// bits are the model's actual stored PK bits — for `HeerIdDesc` that
/// means the XOR-flipped form, which is the canonical wire shape for
/// that PK type. Returns `None` when `T::Id` is some other type
/// (callers gate this via [`t_id_decodes_from_outbox_bigint`]).
fn cast_row_id_to_t_id<TId: 'static>(raw: i64) -> Option<TId> {
    // SAFETY: `HeerId` and `HeerIdDesc` are both `#[repr(transparent)]`
    // over `i64` with identical layout. Each branch is gated on a
    // TypeId equality check. The source `raw` is an owned `i64` (Copy),
    // so transmuting its bits is sound.
    if TypeId::of::<TId>() == TypeId::of::<HeerId>() {
        let h = HeerId::from_i64(raw).ok()?;
        Some(unsafe { std::mem::transmute_copy::<HeerId, TId>(&h) })
    } else if TypeId::of::<TId>() == TypeId::of::<HeerIdDesc>() {
        let h = HeerIdDesc::from_i64(raw).ok()?;
        Some(unsafe { std::mem::transmute_copy::<HeerIdDesc, TId>(&h) })
    } else {
        None
    }
}

/// Owned-substrate fetcher for the `QuerySet::refresh_into` path.
/// Carries the connection pool, an `AuthContext` snapshot, an optional
/// `BasicPredicate<T>` filter, and the LRU-eviction-warn fields. The
/// const-fn-pointer assertion at the bottom of this file pins
/// `Send + Sync + 'static` at the type-system level.
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
    /// Per-fetcher watermark for the outbox-tombstone poll (Pattern 2).
    /// Highest `created_at` already observed in `<table>_outbox`.
    /// `None` on first tick (skips replay; see `fetch_delta`).
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
        // Always-on LRU eviction warn. Outer `load(Acquire)` short-
        // circuits once the warn has fired; the inner `swap(true, AcqRel)`
        // gates the actual emission against two ticks racing on the same
        // `false` flag.
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

        // GH #127: filter pushdown is parked until BasicPredicate exposes
        // a SQL emitter. This warn never fires today because
        // `into_basic_predicate()` always returns `None` (GH #126).
        if self.filter.is_some() {
            tracing::warn!(
                target: "djogi::cache",
                model = std::any::type_name::<T>(),
                "filter pushdown to delta-fetcher SQL emitter is not yet implemented; \
                 refresh tick will fetch the full source-of-truth set within the \
                 watermark window. Tracked at GH #127.",
            );
        }

        // Auth is locked to the subscription (spec §677): the snapshot
        // captured at refresh_into is applied to a fresh ctx below.
        let auth = self.auth.clone();
        let since = query.since.clone();
        let recover_ids = query.recover_ids.clone();
        let watermark_col = <T as DjogiDeltaSyncMeta>::WATERMARK_COLUMN;
        let table_name = <T as crate::model::Model>::table_name();
        let column_list = <T as FromPgRow>::COLUMN_LIST;

        // Pattern 2 gate: model emitted `<table>_outbox` AND `T::Id`
        // decodes from the BIGINT `row_id` column. Default events models
        // use `pk = HeerIdDesc`, so the gate must accept that flavour
        // alongside ascending `HeerId`.
        let outbox_enabled =
            T::descriptor().has_outbox && t_id_decodes_from_outbox_bigint::<T::Id>();
        let outbox_watermark_snapshot: Option<OffsetDateTime> = if outbox_enabled {
            *self
                .outbox_watermark
                .lock()
                .expect("outbox_watermark mutex poisoned")
        } else {
            None
        };

        // Capture the first-tick clock BEFORE the transaction begins so
        // a delete committed between this snapshot and the post-
        // transaction watermark write still has `created_at >= watermark`
        // and gets caught on the next tick. Setting the watermark to a
        // post-transaction `now()` would lose deletes whose `created_at`
        // landed in the gap.
        let first_tick_clock: Option<OffsetDateTime> =
            if outbox_enabled && outbox_watermark_snapshot.is_none() {
                Some(OffsetDateTime::now_utc())
            } else {
                None
            };

        // The SQL must run inside `transaction::atomic` because
        // `auto_set_tenant::<T>` issues `SET LOCAL app.tenant_id`, which
        // only persists inside an open transaction. Without that wrap,
        // RLS-backed tenant isolation would silently fail. The Pattern 2
        // outbox poll piggy-backs on the same transaction so it inherits
        // the same tenant scope.
        // The closure returns the outbox tombstones as `(i64, OffsetDateTime)`
        // — the raw `row_id` bits, not a `HeerId`. The caller decodes
        // them into `T::Id` via `cast_row_id_to_t_id`, which knows
        // about both `HeerId` and `HeerIdDesc` flavours.
        let (items, outbox_tombstones): (Vec<T>, Vec<(i64, OffsetDateTime)>) =
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

                // Pattern 2 outbox poll. `created_at >= $1` is inclusive
                // because sassi's `apply_delta` deduplicates by id and
                // a `>` boundary would drop tombstones with
                // sub-microsecond `created_at` ties. First tick (no
                // watermark) skips so the cache doesn't replay history.
                let outbox_tombstones: Vec<(i64, OffsetDateTime)> = if outbox_enabled
                    && let Some(watermark) = outbox_watermark_snapshot
                {
                    // Defense-in-depth ident check before SQL embedding,
                    // mirroring `outbox/worker.rs::validate_table_ident`.
                    let outbox_table = format!("{table_name}_outbox");
                    crate::ident::check_plain_ident(&outbox_table, false).map_err(|e| {
                        crate::DjogiError::Db(crate::error::DbError::other(format!(
                            "outbox poll: invalid outbox table name {outbox_table:?}: {e:?}"
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
                    let mut decoded: Vec<(i64, OffsetDateTime)> =
                        Vec::with_capacity(rows.len());
                    for row in rows {
                        let raw: i64 = row.try_get(0).map_err(|e| {
                            crate::DjogiError::Db(crate::error::DbError::other(format!(
                                "outbox poll: decode row_id i64: {e}"
                            )))
                        })?;
                        let ts: OffsetDateTime = row.try_get(1).map_err(|e| {
                            crate::DjogiError::Db(crate::error::DbError::other(format!(
                                "outbox poll: decode created_at: {e}"
                            )))
                        })?;
                        decoded.push((raw, ts));
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

        // Pattern 1: derive tombstones from soft-deleted rows. The
        // deletion signal flows through the watermark (NOT a
        // `deleted_at IS NULL` filter — spec §415). For non-soft-
        // deletable models `__delta_should_tombstone()` always returns
        // `false`, so this loop becomes a no-op classification pass.
        let mut live_items = Vec::with_capacity(items.len());
        let mut tombstones: HashSet<T::Id> = HashSet::new();
        for item in items {
            if item.__delta_should_tombstone() {
                tombstones.insert(<T as sassi::Cacheable>::id(&item));
            } else {
                live_items.push(item);
            }
        }

        // Pattern 2: merge outbox-derived tombstones and advance the
        // watermark to `max(created_at)` only after the merge succeeds,
        // so a panic during conversion can't strand events past the
        // watermark unprocessed.
        if !outbox_tombstones.is_empty() {
            let mut max_seen: Option<OffsetDateTime> = None;
            for (raw, ts) in &outbox_tombstones {
                if let Some(t_id) = cast_row_id_to_t_id::<T::Id>(*raw) {
                    tombstones.insert(t_id);
                } else {
                    debug_assert!(
                        false,
                        "outbox poll: cast_row_id_to_t_id returned None despite TypeId gate"
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
                // Monotonic advance — guard against a concurrent tick
                // that already moved the watermark forward.
                let advance = match *guard {
                    None => true,
                    Some(prev) => new_watermark > prev,
                };
                if advance {
                    *guard = Some(new_watermark);
                }
            }
        } else if let Some(initial) = first_tick_clock {
            // First-tick initialisation. Captured BEFORE the transaction
            // ran (see `first_tick_clock` above) so a delete committed
            // during the transaction has `created_at >= initial` on the
            // next tick's poll.
            let mut guard = self
                .outbox_watermark
                .lock()
                .expect("outbox_watermark mutex poisoned");
            if guard.is_none() {
                *guard = Some(initial);
            }
        }

        // High watermark is inferred from `max(item.watermark())` across
        // `live_items` only — tombstoned rows are excluded by virtue of
        // sitting in the separate `tombstones` set, not because sassi
        // filters them. A deletion is therefore not itself a high-water
        // checkpoint, which is the intended behavior.
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
