//! Delta-sync fetcher for `QuerySet::refresh_into` — Cluster 8δ T8.3 skeleton,
//! T8.5 SQL implementation.
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
//! 5. Return `DeltaResult::new(items, HashSet::new())` — tombstones are empty
//!    in this commit; T8.6+ wires tombstone collection.
//! 6. Drop ctx (releases connection back to pool on drop).
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

use crate::auth::AuthContext;
use crate::cache::DjogiDeltaSyncMeta;
use crate::context::DjogiContext;
use crate::pg::decode::FromPgRow;
use crate::pg::pool::DjogiPool;
use sassi::{BasicPredicate, DeltaPunnuFetcher, DeltaQuery, DeltaResult, FetchError};
use std::collections::HashSet;
use std::marker::PhantomData;
use tokio_postgres::types::ToSql;

/// Owned-substrate fetcher for the `QuerySet::refresh_into` path.
///
/// Holds a clone of the connection pool, an `AuthContext` by value, and an
/// optional `BasicPredicate<T>` filter. NEVER references `&mut DjogiContext`.
///
/// # Send + Sync
///
/// Auto-derived: every field is `Send + Sync` when `T: Send + Sync`, and
/// `PhantomData<T>` contributes no additional bounds. No manual `unsafe impl`
/// was required.
pub(crate) struct DjogiDeltaFetcher<T: sassi::DeltaSyncCacheable> {
    pub(crate) pool: DjogiPool,
    pub(crate) auth: AuthContext,
    pub(crate) filter: Option<BasicPredicate<T>>,
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

        // ── 1. Acquire a connection from the pool ────────────────────────────
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| FetchError::Custom(Box::new(e)))?;

        // ── 2. Build a fresh DjogiContext with captured auth ─────────────────
        // Per spec §677: auth is locked to the subscription — the context uses
        // the AuthContext snapshot captured at refresh_into call time, not
        // whatever the caller's context holds at tick time.
        let mut ctx = DjogiContext::from_connection(conn).with_auth(self.auth.clone());

        // ── 3. Build SQL ─────────────────────────────────────────────────────
        let watermark_col = <T as DjogiDeltaSyncMeta>::WATERMARK_COLUMN;
        let table_name = <T as crate::model::Model>::table_name();
        let column_list = <T as FromPgRow>::COLUMN_LIST;

        // Collect runtime-typed bind values. We box each value into a
        // `Box<dyn ToSql + Sync + Send>` so we can collect heterogeneous types
        // (T::Watermark and T::Id may differ) into one Vec. The references for
        // raw_query are derived from the boxes after the Vec is complete.
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        let mut where_parts: Vec<String> = Vec::new();

        // Watermark clause: WHERE <col> >= $N (inclusive per DeltaPunnuFetcher
        // contract — boundary rows may have changed without their watermark
        // advancing; Sassi deduplicates by id).
        if let Some(since) = query.since.as_ref() {
            params.push(Box::new(since.clone()));
            where_parts.push(format!("{watermark_col} >= ${}", params.len()));
        }

        // Recovery clause: id IN ($N, $N+1, …)
        // Recovery ids are OR'd with the watermark clause — we want those rows
        // regardless of whether their watermark advanced.
        if !query.recover_ids.is_empty() {
            let mut placeholders: Vec<String> = Vec::new();
            for id in &query.recover_ids {
                params.push(Box::new(id.clone()));
                placeholders.push(format!("${}", params.len()));
            }
            let recover_clause = format!("id IN ({})", placeholders.join(", "));

            if where_parts.is_empty() {
                where_parts.push(recover_clause);
            } else {
                // Combine: (watermark_clause) OR (recovery_clause).
                // The watermark_clause was pushed as a single entry above.
                let watermark_clause = where_parts.remove(0);
                where_parts.push(format!("({watermark_clause}) OR ({recover_clause})"));
            }
        }

        let where_sql = if where_parts.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_parts.join(" AND "))
        };

        let sql =
            format!("SELECT {column_list} FROM {table_name} {where_sql} ORDER BY {watermark_col}");

        // ── 4. Execute ───────────────────────────────────────────────────────
        // Build the `&[&(dyn ToSql + Sync)]` slice raw_query expects.
        // The Vec<Box<...>> owns the values; the slice borrows them.
        let params_refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();

        let items: Vec<T> = ctx
            .raw_query::<T>(&sql, &params_refs)
            .await
            .map_err(|e| FetchError::Custom(Box::new(e)))?;

        // ── 5. Empty tombstones (T8.6+ wires tombstone collection) ───────────
        let tombstones: HashSet<T::Id> = HashSet::new();

        // ── 6. ctx drops here — returns conn back to pool ────────────────────
        drop(ctx);

        Ok(DeltaResult::new(items, tombstones))
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
