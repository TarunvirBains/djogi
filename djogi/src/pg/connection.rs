//! `PgConnection` — a deadpool-managed Postgres connection with a per-connection
//! statement cache.
//!
//! # What
//!
//! `PgConnection` wraps a `deadpool_postgres::Object` (a checked-out pool
//! connection) and adds a per-connection `HashMap`-based statement cache keyed
//! by SQL text.
//!
//! # Statement cache
//!
//! On each `prepare_cached(sql)` call, the accumulator-produced SQL text is used
//! to prepare the statement if it has not been seen before. Subsequent calls with
//! the same SQL string return a clone of the cached `tokio_postgres::Statement`
//! without a round-trip to the server.
//!
//! Cache invalidation is not needed: deadpool recycles connections by dropping
//! the `Object` and creating a new one. `PgConnection` is re-constructed from
//! the new `Object`, and the `cache` field is initialized as an empty `HashMap`.
//!
//! # Transaction handling
//!
//! A `PgConnection` with an active transaction is represented by a
//! `ContextInner::Transaction(PgConnection)` in `DjogiContext`. The `BEGIN` /
//! `COMMIT` / `ROLLBACK` commands are issued via `batch_execute` on the inner
//! `Object`. `SAVEPOINT` / `RELEASE SAVEPOINT` / `ROLLBACK TO SAVEPOINT` for
//! nested `atomic()` scopes also go through `batch_execute`.
//!
//! # `Send + !Sync`
//!
//! `tokio_postgres::Client` (and therefore `deadpool_postgres::Object`) is `Send`
//! but not `Sync`. `PgConnection` inherits these bounds. This matches the
//! `sqlx::Transaction<'static, Postgres>` it replaces.

use crate::DjogiError;
use deadpool_postgres::Object;
use postgres_types::ToSql;
use std::collections::HashMap;
use tokio_postgres::{Row, Statement};

/// A checked-out Postgres connection from the pool, with a per-connection
/// statement cache.
pub struct PgConnection {
    /// The deadpool-managed connection object. Returned to the pool on drop.
    obj: Object,
    /// Per-connection statement cache keyed by SQL text. Populated lazily
    /// on the first `prepare_cached(sql)` call for each distinct SQL string.
    cache: HashMap<String, Statement>,
}

impl PgConnection {
    /// Wrap a `deadpool_postgres::Object` in a `PgConnection` with an empty cache.
    pub fn new(obj: Object) -> Self {
        PgConnection {
            obj,
            cache: HashMap::new(),
        }
    }

    /// Prepare `sql` if not already cached; return a clone of the statement.
    ///
    /// The statement is stored in the per-connection `cache` so that repeated
    /// queries with the same SQL string avoid a prepare round-trip. Cache entries
    /// live for the connection's lifetime; the cache is dropped when the
    /// `PgConnection` is dropped (which returns the `Object` to the deadpool pool).
    pub async fn prepare_cached(&mut self, sql: &str) -> Result<Statement, DjogiError> {
        if let Some(stmt) = self.cache.get(sql) {
            return Ok(stmt.clone());
        }
        let stmt = self
            .obj
            .prepare(sql)
            .await
            .map_err(|e| DjogiError::Sqlx(sqlx::Error::Protocol(e.to_string())))?;
        self.cache.insert(sql.to_owned(), stmt.clone());
        Ok(stmt)
    }

    /// Execute `sql` as a `SIMPLE QUERY` (no bind parameters). Used for
    /// `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT sp_n`, `RELEASE SAVEPOINT sp_n`,
    /// and `ROLLBACK TO SAVEPOINT sp_n`. These control commands never carry
    /// user-supplied values so the simple query protocol is appropriate.
    pub async fn batch_execute(&mut self, sql: &str) -> Result<(), DjogiError> {
        self.obj.batch_execute(sql).await.map_err(pg_err_to_djogi)
    }

    /// Execute a parameterised query and return all rows.
    ///
    /// Prepares `sql` (or retrieves from cache), then calls
    /// `tokio_postgres::Client::query` with the provided parameters.
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        let stmt = self.prepare_cached(sql).await?;
        self.obj.query(&stmt, params).await.map_err(pg_err_to_djogi)
    }

    /// Execute a parameterised query and return the first row, if any.
    pub async fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        let stmt = self.prepare_cached(sql).await?;
        self.obj
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
        self.obj
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
        self.obj
            .execute(&stmt, params)
            .await
            .map_err(pg_err_to_djogi)
    }
}

// ---------------------------------------------------------------------------
// Error conversion helpers
// ---------------------------------------------------------------------------

/// Convert a `tokio_postgres::Error` into a `DjogiError`.
///
/// In T2 this maps to `DjogiError::Sqlx` (keeping the existing variant shape).
/// T6 renames the variant to `DjogiError::Db`. The lock-conflict classification
/// that previously lived in `error::is_lock_error` is ported to operate on
/// `tokio_postgres::error::SqlState` in T2's `error.rs`.
pub(crate) fn pg_err_to_djogi(e: tokio_postgres::Error) -> DjogiError {
    crate::error::map_pg_err(e)
}
