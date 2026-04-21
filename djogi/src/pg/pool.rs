//! `DjogiPool` — a thin wrapper around `deadpool_postgres::Pool`.
//!
//! # What
//!
//! `DjogiPool` is the framework's connection pool type. It wraps a
//! `deadpool_postgres::Pool` and exposes the two operations the framework
//! needs:
//!
//! - `connect(url)` — construct a pool from a connection URL (max size = 5
//!   for T2; real pool-config wiring deferred to Phase 5-One).
//! - `get()` — acquire a `PgConnection` from the pool.
//!
//! The inner pool is `pub(crate)` — not exposed publicly. Public escape-hatch
//! access to the underlying deadpool pool is deferred to T5's `ctx.raw_*`
//! surface.
//!
//! # `Clone` and `Send + Sync`
//!
//! `deadpool_postgres::Pool` is `Clone` (internally `Arc`-backed) and
//! `Send + Sync`. `DjogiPool` inherits all three properties, which means it
//! can be shared across tasks without wrapping in `Arc` — matching the
//! `sqlx::PgPool` behaviour it replaces.

use crate::DjogiError;
use crate::pg::connection::PgConnection;
use deadpool_postgres::{Config, ManagerConfig, PoolConfig, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;

/// The framework's Postgres connection pool.
///
/// Wraps `deadpool_postgres::Pool` with a Djogi-specific constructor.
/// `Clone` is free (the underlying pool is `Arc`-backed).
#[derive(Clone)]
pub struct DjogiPool {
    /// The underlying deadpool-postgres pool. `pub(crate)` for framework
    /// internals; no public access — escape-hatch deferred to T5.
    pub(crate) inner: deadpool_postgres::Pool,
}

impl DjogiPool {
    /// Build a `DjogiPool` from a Postgres connection URL.
    ///
    /// Uses `max_size = 5` for T2. Real pool-config wiring (from `Djogi.toml`
    /// or env vars) is deferred to Phase 5-One.
    ///
    /// The URL format is the standard Postgres connection string, e.g.
    /// `postgres://user:pass@localhost:5432/dbname`.
    pub async fn connect(url: &str) -> Result<Self, DjogiError> {
        let mut cfg = Config::new();
        cfg.url = Some(url.to_owned());
        cfg.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });
        // T2: hardcoded max_size = 5. Phase 5-One wires this from Djogi.toml.
        cfg.pool = Some(PoolConfig::new(5));

        let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls).map_err(|e| {
            DjogiError::Sqlx(sqlx::Error::Configuration(
                format!("DjogiPool::connect: pool creation failed: {e}").into(),
            ))
        })?;

        Ok(DjogiPool { inner: pool })
    }

    /// Acquire a `PgConnection` from the pool.
    ///
    /// The returned `PgConnection` holds the connection for the caller's
    /// lifetime and returns it to the pool on drop. If the pool has no idle
    /// connections and is at max capacity, this waits until one is available
    /// (deadpool default: indefinitely — callers that need a timeout should
    /// set `cfg.pool.timeout` before constructing the pool).
    pub(crate) async fn get(&self) -> Result<PgConnection, DjogiError> {
        let obj =
            self.inner.get().await.map_err(|e| {
                DjogiError::Sqlx(sqlx::Error::PoolClosed).context_msg_unreachable(e)
            })?;
        Ok(PgConnection::new(obj))
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

trait ContextMsgUnreachable {
    fn context_msg_unreachable(self, _e: impl std::fmt::Display) -> Self;
}

impl ContextMsgUnreachable for DjogiError {
    fn context_msg_unreachable(self, e: impl std::fmt::Display) -> Self {
        // We cannot easily attach context to sqlx::Error variants after
        // construction; log the pool error and return the Sqlx variant.
        tracing::error!("DjogiPool::get failed: {e}");
        self
    }
}
