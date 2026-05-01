//! `DjogiPool` — the framework's Postgres connection pool.
//!
//! # What
//!
//! `DjogiPool` wraps a `deadpool_postgres::Pool` and is the single way Djogi
//! checks out a Postgres connection. The type is `Clone` (the inner pool is
//! `Arc`-backed) and `Send + Sync`, so a `DjogiPool` can be shared across
//! tasks without external wrapping.
//!
//! # Why a public ergonomic surface
//!
//! `DjogiPool::connect(url)` is fine for development, but production
//! services need to size the pool against their concurrency budget and
//! bound the wait for a slot so a saturated pool fails fast instead of
//! queueing forever. Phase 8-Zero introduces [`DjogiPool::builder`], which
//! exposes `.max_size` and `.timeout` knobs and is the canonical entry
//! point for non-default construction. `connect(url)` is preserved as
//! sugar for `DjogiPool::builder(url).build().await`.
//!
//! Subsequent tasks in this cluster add a `post_connect` setup hook (T3)
//! and a `with_client` raw-borrow helper (T4); this file lands the
//! foundational builder shape first.

use crate::pg::connection::PgConnection;
use crate::{DbError, DjogiError};
use deadpool_postgres::{Config, ManagerConfig, PoolConfig, RecyclingMethod, Runtime};
use std::time::Duration;
use tokio_postgres::NoTls;

/// Default `max_size` when the caller does not override it.
///
/// Five matches the original Phase-1 hard-coded value and keeps every
/// existing test/callsite that uses [`DjogiPool::connect`] running against
/// the same pool size as before. Production deployments override this
/// through [`DjogiPoolBuilder::max_size`].
pub const DEFAULT_MAX_SIZE: usize = 5;

/// The framework's Postgres connection pool.
///
/// Wraps `deadpool_postgres::Pool` with a Djogi-specific constructor.
/// `Clone` is free — the underlying pool is `Arc`-backed, so a clone bumps
/// a refcount rather than copying the pool state.
#[derive(Clone)]
pub struct DjogiPool {
    /// The underlying deadpool-postgres pool. `pub(crate)` so internal
    /// substrate (`context`, `transaction`, `live_migrate`, `outbox`) can
    /// reach the inner pool without making the whole inner type part of
    /// the public API surface. Adopter code goes through the public
    /// methods on `DjogiPool` and through `DjogiContext`, never through
    /// `inner`.
    pub(crate) inner: deadpool_postgres::Pool,
}

impl std::fmt::Debug for DjogiPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DjogiPool")
            .field("status", &self.inner.status())
            .finish_non_exhaustive()
    }
}

impl DjogiPool {
    /// Build a `DjogiPool` from a Postgres connection URL using framework
    /// defaults.
    ///
    /// Equivalent to `DjogiPool::builder(url).build().await`. Defaults:
    ///
    /// - `max_size` = [`DEFAULT_MAX_SIZE`] (5)
    /// - no wait timeout (callers block until a slot is available)
    ///
    /// For tunable size or timeouts use [`DjogiPool::builder`] instead.
    pub async fn connect(url: &str) -> Result<Self, DjogiError> {
        Self::builder(url).build().await
    }

    /// Start configuring a `DjogiPool` against the given Postgres URL.
    ///
    /// The URL format is the standard Postgres connection string, e.g.
    /// `postgres://user:pass@localhost:5432/dbname`.
    ///
    /// The returned [`DjogiPoolBuilder`] exposes `.max_size` and
    /// `.timeout`, finalised by `.build().await`.
    ///
    /// ```ignore
    /// let pool = DjogiPool::builder("postgres://localhost/app")
    ///     .max_size(20)
    ///     .timeout(Duration::from_secs(5))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn builder(url: impl Into<String>) -> DjogiPoolBuilder {
        DjogiPoolBuilder::new(url.into())
    }

    /// Acquire a `PgConnection` from the pool.
    ///
    /// The returned `PgConnection` holds the connection for the caller's
    /// lifetime and returns it to the pool on drop. If the pool has no idle
    /// connections and is at max capacity, this waits until one is available
    /// (deadpool default: indefinitely; bound the wait via
    /// [`DjogiPoolBuilder::timeout`]).
    pub(crate) async fn get(&self) -> Result<PgConnection, DjogiError> {
        let obj = self.inner.get().await.map_err(map_pool_err)?;
        Ok(PgConnection::new(obj))
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`DjogiPool`].
///
/// Construct via [`DjogiPool::builder`]. Every field has a sensible default;
/// `.build().await` finalises into a usable pool.
pub struct DjogiPoolBuilder {
    url: String,
    max_size: usize,
    wait_timeout: Option<Duration>,
}

impl DjogiPoolBuilder {
    /// Internal constructor — adopter code reaches this through
    /// [`DjogiPool::builder`].
    fn new(url: String) -> Self {
        Self {
            url,
            max_size: DEFAULT_MAX_SIZE,
            wait_timeout: None,
        }
    }

    /// Set the maximum number of connections the pool will hold.
    ///
    /// `max_size` is the hard cap on physical connections. Once the pool
    /// has handed out `max_size` connections, additional `get` requests
    /// queue until a checkout is returned (or the wait timeout fires, if
    /// configured via [`Self::timeout`]).
    ///
    /// Default: [`DEFAULT_MAX_SIZE`].
    ///
    /// `value` must be `>= 1`. Passing `0` would build a pool whose
    /// internal semaphore has zero permits, so every `pool.get()` would
    /// block forever (or until the wait timeout fires). The check runs
    /// in [`Self::build`] so the failure surfaces at construction time
    /// with a clear error rather than as a mysterious hang at first
    /// query.
    ///
    /// Sizing guidance: pick `max_size` to match your service's expected
    /// concurrent database-touching tasks, NOT your CPU count. A web
    /// server handling 200 concurrent requests that each issue 2-3
    /// sequential queries needs roughly 30-50 connections, not 8.
    pub fn max_size(mut self, value: usize) -> Self {
        self.max_size = value;
        self
    }

    /// Set the maximum time `pool.get` (and the implicit get inside every
    /// terminal query) will wait for a slot before returning
    /// [`DjogiError::PoolTimeout`].
    ///
    /// Default: no timeout — callers wait indefinitely. Production
    /// services typically pick a budget in the 1-10 second range so a
    /// saturated pool fails fast instead of accumulating unbounded queue
    /// depth.
    ///
    /// This sets deadpool's `wait` timeout (waiting for a free slot). The
    /// `create` and `recycle` timeouts are independent and not exposed
    /// through this builder; they default to "no timeout" and are managed
    /// by deadpool internally.
    pub fn timeout(mut self, value: Duration) -> Self {
        self.wait_timeout = Some(value);
        self
    }

    /// Finalise the builder into a usable [`DjogiPool`].
    ///
    /// This constructs the deadpool pool eagerly — the `tokio` runtime
    /// must be available when `build` is awaited. The pool itself opens
    /// connections lazily on first checkout.
    ///
    /// # Errors
    ///
    /// Returns [`DjogiError::Db`] with a `Validation`-style message if
    /// `max_size` is `0` (a zero-permit pool would hang every `get` call
    /// forever) or if the underlying `deadpool` config fails to build.
    pub async fn build(self) -> Result<DjogiPool, DjogiError> {
        if self.max_size == 0 {
            return Err(DjogiError::Validation(
                "DjogiPoolBuilder::max_size must be >= 1; \
                 a zero-permit pool would block every checkout indefinitely"
                    .to_owned(),
            ));
        }

        let mut cfg = Config::new();
        cfg.url = Some(self.url);
        cfg.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });
        cfg.pool = Some(PoolConfig::new(self.max_size));

        // `cfg.builder()` returns the deadpool `PoolBuilder`. We finish it
        // off ourselves so we can attach `wait_timeout` — the high-level
        // `cfg.create_pool` does not expose it.
        let mut pool_builder = cfg.builder(NoTls).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "DjogiPool builder: invalid config — {e}"
            )))
        })?;
        pool_builder = pool_builder.runtime(Runtime::Tokio1);

        if let Some(d) = self.wait_timeout {
            pool_builder = pool_builder.wait_timeout(Some(d));
        }

        let pool = pool_builder.build().map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "DjogiPool::build: pool creation failed: {e}"
            )))
        })?;

        Ok(DjogiPool { inner: pool })
    }
}

impl std::fmt::Debug for DjogiPoolBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DjogiPoolBuilder")
            .field("max_size", &self.max_size)
            .field("wait_timeout", &self.wait_timeout)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Lower a `deadpool_postgres::PoolError` into a `DjogiError`, mapping
/// the timeout variants to [`DjogiError::PoolTimeout`] so callers can
/// match on them without inspecting the deadpool error type.
pub(crate) fn map_pool_err(e: deadpool_postgres::PoolError) -> DjogiError {
    use deadpool_postgres::PoolError as P;
    match e {
        P::Timeout(deadpool_postgres::TimeoutType::Wait) => {
            DjogiError::PoolTimeout { phase: "wait" }
        }
        P::Timeout(deadpool_postgres::TimeoutType::Create) => {
            DjogiError::PoolTimeout { phase: "create" }
        }
        P::Timeout(deadpool_postgres::TimeoutType::Recycle) => {
            DjogiError::PoolTimeout { phase: "recycle" }
        }
        other => {
            tracing::error!("DjogiPool: deadpool error: {other}");
            DjogiError::Db(DbError::other(format!("pool error: {other}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction-only smoke test — the builder builds a pool against an
    /// obviously-bogus URL because deadpool defers physical connection
    /// until the first checkout. The test asserts the builder API itself
    /// does not panic and that `Debug` works.
    #[tokio::test]
    async fn builder_constructs_pool_with_defaults() {
        let pool = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .build()
            .await
            .expect("builder should construct pool eagerly without connecting");
        let _ = format!("{pool:?}");
    }

    /// `connect(url)` keeps the pre-builder shape — same defaults, same
    /// return type, same async signature.
    #[tokio::test]
    async fn connect_delegates_to_builder() {
        let _pool = DjogiPool::connect("postgres://localhost/_djogi_unreachable")
            .await
            .expect("connect should construct pool with builder defaults");
    }

    /// `max_size` and `timeout` are accepted without runtime panic; the
    /// resulting pool's debug output reflects the configured cap.
    #[tokio::test]
    async fn builder_accepts_max_size_and_timeout() {
        let pool = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(7)
            .timeout(Duration::from_millis(50))
            .build()
            .await
            .expect("builder should accept max_size and timeout");
        let dbg = format!("{pool:?}");
        // `Pool::status()` exposes `max_size` — the Debug impl includes it.
        assert!(
            dbg.contains("max_size: 7"),
            "Debug output should reflect max_size, got: {dbg}"
        );
    }

    /// A zero `max_size` would build a pool whose semaphore has no
    /// permits, hanging every checkout forever. Reject at `build` time.
    #[tokio::test]
    async fn builder_rejects_zero_max_size() {
        let err = DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(0)
            .build()
            .await
            .expect_err("max_size(0) must be rejected");
        match err {
            DjogiError::Validation(msg) => {
                assert!(
                    msg.contains("max_size") && msg.contains(">= 1"),
                    "Validation message should mention max_size and >= 1; got: {msg}"
                );
            }
            other => panic!("expected DjogiError::Validation, got: {other:?}"),
        }
    }

    /// `map_pool_err` lowers each deadpool timeout variant into the
    /// matching `DjogiError::PoolTimeout { phase }`.
    #[test]
    fn map_pool_err_lowers_timeouts() {
        use deadpool_postgres::{PoolError, TimeoutType};

        match map_pool_err(PoolError::Timeout(TimeoutType::Wait)) {
            DjogiError::PoolTimeout { phase } => assert_eq!(phase, "wait"),
            other => panic!("expected PoolTimeout(wait), got: {other:?}"),
        }
        match map_pool_err(PoolError::Timeout(TimeoutType::Create)) {
            DjogiError::PoolTimeout { phase } => assert_eq!(phase, "create"),
            other => panic!("expected PoolTimeout(create), got: {other:?}"),
        }
        match map_pool_err(PoolError::Timeout(TimeoutType::Recycle)) {
            DjogiError::PoolTimeout { phase } => assert_eq!(phase, "recycle"),
            other => panic!("expected PoolTimeout(recycle), got: {other:?}"),
        }
    }

    /// PoolTimeout is classified as transient — generic retry helpers
    /// must treat it as a back-off-and-retry condition, not a permanent
    /// failure.
    #[test]
    fn pool_timeout_is_transient() {
        let err = DjogiError::PoolTimeout { phase: "wait" };
        assert!(err.is_transient(), "PoolTimeout must be transient");
        assert!(!err.is_terminal(), "PoolTimeout must NOT be terminal");
    }
}
