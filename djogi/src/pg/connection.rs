//! `PgConnection` — a deadpool-managed Postgres connection with a per-connection
//! statement cache.
//! # What
//! `PgConnection` wraps a `deadpool_postgres::Object` (a checked-out pool
//! connection) and delegates statement caching to `deadpool_postgres`'s own
//! built-in [`deadpool_postgres::StatementCache`] via
//! [`deadpool_postgres::ClientWrapper::prepare_cached`].
//! # Statement cache
//! `deadpool_postgres::ClientWrapper` owns a `StatementCache` that lives with
//! the underlying connection — not with each checkout. When the same
//! `ClientWrapper` is checked out a second time (after being returned to the
//! pool), the populated cache is still there. When deadpool drops and recreates
//! the connection due to a recycle failure (connection closed / I/O error), the
//! old `ClientWrapper` and its `StatementCache` are discarded and a fresh one is
//! allocated — exactly the invalidation semantics RQ-2 requires.
//! `Object` implements `Deref<Target = ClientWrapper>`, so calling
//! `self.obj.prepare_cached(sql)` goes through `ClientWrapper::prepare_cached`.
//! # TODO: bound cache size + LRU eviction
//! The `StatementCache` is currently unbounded. A future change should
//! cap it and apply LRU eviction to prevent memory growth on schemas
//! with very many distinct query shapes.
//! # Transaction handling
//! A `PgConnection` with an active transaction is represented by a
//! `ContextInner::Transaction(PgConnection)` in `DjogiContext`. The `BEGIN` /
//! `COMMIT` / `ROLLBACK` commands are issued via `batch_execute` on the inner
//! `Object`. `SAVEPOINT` / `RELEASE SAVEPOINT` / `ROLLBACK TO SAVEPOINT` for
//! nested `atomic()` scopes also go through `batch_execute`.
//! # `Send + !Sync`
//! `tokio_postgres::Client` (and therefore `deadpool_postgres::Object`) is `Send`
//! but not `Sync`. `PgConnection` inherits these bounds — the same contract
//! as other async Postgres wrappers that encapsulate a network connection.

use crate::{DbError, DjogiError};
use deadpool_postgres::Object;
use postgres_types::ToSql;
use tokio_postgres::{Row, Statement};

/// A checked-out Postgres connection from the pool, with a per-connection
/// statement cache.
/// The cache is owned by the underlying `deadpool_postgres::ClientWrapper`, not
/// by this checkout wrapper. The same `ClientWrapper` may be checked out
/// multiple times; its `StatementCache` accumulates entries across all checkouts.
/// When deadpool recycles the connection (e.g. I/O error, `is_closed()` true),
/// the `ClientWrapper` is dropped and a new one created — clearing the cache
/// automatically. See the module-level docs for the full cache lifecycle.
pub struct PgConnection {
    /// The deadpool-managed connection object. Returned to the pool on drop.
    /// `None` after [`Self::detach`] or [`Self::detach_mut`] has been called.
    obj: Option<Object>,
}

#[allow(clippy::disallowed_methods)]
impl PgConnection {
    /// Wrap a `deadpool_postgres::Object` in a `PgConnection`.
    /// No per-checkout cache is allocated — statement caching is delegated to
    /// the `StatementCache` embedded in the underlying `ClientWrapper`.
    pub fn new(obj: Object) -> Self {
        PgConnection { obj: Some(obj) }
    }

    /// Detach this connection from its pool instead of returning it.
    /// Consumes `self`, calls `deadpool_postgres::Object::take` to remove the
    /// underlying `ClientWrapper` from the pool's tracker, and drops it
    /// closing the `tokio_postgres::Client` and the underlying socket. The
    /// pool will create a fresh physical connection on the next demand.
    /// This is the dirty-exit branch of [`crate::context::PoolConnGuard`],
    /// the parallel of [`crate::pg::pool::DjogiPool::with_client`]'s
    /// `WithClientGuard` discard. See `WithClientGuard` for the underlying
    /// rationale (deadpool's `RecyclingMethod::Fast` would otherwise leak a
    /// poisoned session to the next checkout).
    pub(crate) fn detach(self) {
        if let Some(obj) = self.obj {
            // `_client_wrapper` drops at end of scope, closing the underlying
            // connection.
            let _client_wrapper = Object::take(obj);
        }
    }

    /// Detach this connection from its pool without consuming `self`.
    /// Extracts the `Object`, removes it from the pool's tracker via
    /// `Object::take`, and drops the underlying connection. The struct is left
    /// in a vacant state (`obj: None`). Subsequent method calls will panic.
    /// Used by [`crate::context::pinned_ctx::OwnedPinnedCtx`] which only has
    /// `&mut self` in its `Drop` impl (GH #331).
    pub(crate) fn detach_mut(&mut self) {
        if let Some(obj) = std::mem::take(&mut self.obj) {
            // `_client_wrapper` drops at end of scope, closing the underlying
            // connection and removing it from the pool's tracker.
            let _client_wrapper = Object::take(obj);
        }
    }

    /// Inner helper to get a mutable reference to the wrapped `Object`.
    fn obj_mut(&mut self) -> &mut Object {
        self.obj.as_mut().expect("PgConnection used after detach")
    }

    /// Prepare `sql` if not already cached; return a clone of the statement.
    /// Delegates to `deadpool_postgres::ClientWrapper::prepare_cached`, which
    /// checks the connection-local `StatementCache` before issuing a prepare
    /// round-trip. The cache entry lives for the lifetime of the underlying
    /// connection, surviving across pool checkouts of the same `ClientWrapper`.
    pub async fn prepare_cached(&mut self, sql: &str) -> Result<Statement, DjogiError> {
        self.obj_mut()
            .prepare_cached(sql)
            .await
            .map_err(|e| DjogiError::Db(DbError::other(e.to_string())))
    }

    /// Execute `sql` as a `SIMPLE QUERY` (no bind parameters). Used for
    /// `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT sp_n`, `RELEASE SAVEPOINT sp_n`,
    /// and `ROLLBACK TO SAVEPOINT sp_n`. These control commands never carry
    /// user-supplied values so the simple query protocol is appropriate.
    pub async fn batch_execute(&mut self, sql: &str) -> Result<(), DjogiError> {
        self.obj_mut()
            .batch_execute(sql)
            .await
            .map_err(pg_err_to_djogi)
    }

    /// Execute a parameterised query and return all rows.
    /// Prepares `sql` (or retrieves from cache), then calls
    /// `tokio_postgres::Client::query` with the provided parameters.
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        let stmt = self.prepare_cached(sql).await?;
        self.obj_mut()
            .query(&stmt, params)
            .await
            .map_err(pg_err_to_djogi)
    }

    /// Execute a parameterised query and return the first row, if any.
    pub async fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        let stmt = self.prepare_cached(sql).await?;
        self.obj_mut()
            .query_opt(&stmt, params)
            .await
            .map_err(pg_err_to_djogi)
    }

    /// Execute a parameterised query and return the first row, failing if zero
    /// or more than one row is returned.
    pub async fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError> {
        let stmt = self.prepare_cached(sql).await?;
        self.obj_mut()
            .query_one(&stmt, params)
            .await
            .map_err(pg_err_to_djogi)
    }

    /// Execute a parameterised DML statement (INSERT / UPDATE / DELETE) and
    /// return the number of rows affected.
    pub async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        let stmt = self.prepare_cached(sql).await?;
        self.obj_mut()
            .execute(&stmt, params)
            .await
            .map_err(pg_err_to_djogi)
    }
}

// ---------------------------------------------------------------------------
// Error conversion helpers
// ---------------------------------------------------------------------------

/// Convert a `tokio_postgres::Error` into a `DjogiError`.
/// Routes through [`crate::error::map_pg_err`], which classifies retryable
/// SQLSTATEs into `DjogiError::LockConflict` and everything else into
/// `DjogiError::Db`.
pub(crate) fn pg_err_to_djogi(e: tokio_postgres::Error) -> DjogiError {
    crate::error::map_pg_err(e)
}
