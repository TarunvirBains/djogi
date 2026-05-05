//! Delta-sync fetcher for `QuerySet::refresh_into` — Cluster 8δ T8.3 skeleton.
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
//! # Skeleton scope (T8.3)
//!
//! `fetch_delta`'s body is `unimplemented!("T8.5 implements the SQL path")`.
//! Calling `handle.update().await` panics. T8.5 lands the real SQL path:
//! acquire conn → set up tx-scoped DjogiContext → run fetch with filter +
//! `since` watermark → return `DeltaResult<T, T::Watermark>`.

use crate::auth::AuthContext;
use crate::pg::pool::DjogiPool;
use sassi::{BasicPredicate, DeltaPunnuFetcher, DeltaQuery, DeltaResult, FetchError};
use std::marker::PhantomData;

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
// T8.5 will use `pool`, `auth`, and `filter` in `fetch_delta`.
#[allow(dead_code)]
pub(crate) struct DjogiDeltaFetcher<T: sassi::DeltaSyncCacheable> {
    pub(crate) pool: DjogiPool,
    pub(crate) auth: AuthContext,
    pub(crate) filter: Option<BasicPredicate<T>>,
    pub(crate) _model: PhantomData<T>,
}

#[async_trait::async_trait]
impl<T: sassi::DeltaSyncCacheable + Send + Sync + 'static> DeltaPunnuFetcher<T>
    for DjogiDeltaFetcher<T>
{
    async fn fetch_delta(
        &self,
        _query: DeltaQuery<T>,
    ) -> Result<DeltaResult<T, T::Watermark>, FetchError> {
        // T8.5 implements the SQL path:
        //   1. acquire connection from self.pool
        //   2. construct a tx-scoped DjogiContext + apply self.auth
        //   3. issue SQL with self.filter + _query.since watermark
        //   4. return DeltaResult { items, tombstones, high_watermark }
        unimplemented!("T8.5 implements the SQL path — T8.3 lands the skeleton only")
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
