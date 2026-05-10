//! Deliberate raw SQL escape hatches.
//!
//! This module is public so adopter crates and workspace examples can opt in
//! consciously, but it is `#[doc(hidden)]` and sealed. In this repository's
//! tests, the supported way to bring these traits into scope is the
//! `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute plus an
//! attached `// JUSTIFICATION ...` comment; `cargo xtask check-justifications`
//! enforces that convention under `tests/`.
//!
//! The seal prevents downstream crates from implementing these traits for their
//! own types. The only purpose of the traits is to move djogi-owned raw SQL
//! methods off the ordinary inherent API surface while preserving an explicit
//! opt-in path for pin tests, internal substrate, and truly exceptional adopter
//! needs.
//!
//! # Connection lifecycle — dirty-by-default
//!
//! The pool-backed raw methods on
//! [`RawAccessExt`](RawAccessExtBase) (`raw_query`, `raw_rows`,
//! `raw_fetch_one`, `raw_scalar`, `raw_execute`, `raw_ddl`) acquire a
//! pooled connection through [`crate::context::DjogiContext`]'s execution
//! helpers, which wrap each checkout in a dirty-by-default guard:
//!
//! - **Clean exit (`Ok`).** The connection returns to the pool the
//!   normal way; the next checkout reuses it.
//! - **Dirty exit (`Err`, panic, future cancellation).** The connection
//!   is detached via `deadpool_postgres::Object::take` and dropped
//!   immediately, closing the underlying `tokio_postgres::Client` and
//!   socket. The pool will create a fresh physical connection on the
//!   next demand. The trade-off is one extra physical connection per
//!   dirty exit, paid for the guarantee that a poisoned session
//!   (open transaction, uncommitted `SET ROLE`, `SET search_path`,
//!   advisory lock, half-finished `COPY` stream) cannot leak to the
//!   next checkout.
//!
//! This is the same lifecycle [`crate::pg::pool::DjogiPool::with_client`]
//! enforces via its `WithClientGuard`. It is required because Djogi runs
//! its pools with `deadpool_postgres::RecyclingMethod::Fast`, which only
//! checks `is_closed()` on return — it does **not** issue `ROLLBACK`,
//! `RESET ALL`, or `DISCARD ALL`.
//!
//! ## Adopter contract
//!
//! Even with the dirty-by-default guard, raw SQL that mutates session
//! state (`SET ROLE`, `SET search_path`, advisory locks, manual
//! `BEGIN`/`COMMIT`, `LISTEN`/`UNLISTEN`, prepared-statement creation
//! outside the cache) on the **clean-exit path** still leaves the
//! connection in a non-default state when it returns to the pool. Wrap
//! such SQL in [`crate::transaction::atomic`] so the surrounding
//! transaction's commit or rollback bounds the state change, or use the
//! transaction-local form (`SET LOCAL …`, `set_config(name, value, true)`,
//! `BEGIN; … COMMIT;`) inside the closure.
//!
//! Cursors, `COPY` streams, and other multi-round-trip protocol
//! operations should run through
//! [`RawPoolAccessExt::raw_with_client`](RawPoolAccessExtBase) — the
//! `WithClientGuard` there bounds the protocol exchange to a single
//! checkout and applies the same dirty-detach on dirty exit.
//!
//! Tracking issue: [djogi#162](https://github.com/TarunvirBains/djogi/issues/162).
//! See also [`docs/spec/raw-sql-escape-hatches.md`](https://github.com/TarunvirBains/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md)
//! for the full contract.

use crate::context::DjogiContext;
use crate::pg::connection::PgConnection;
use crate::pg::decode::{FromPgRow, try_get_scalar};
use crate::pg::pool::{ClientFuture, DjogiPool};
use crate::query::stream::{DEFAULT_FETCH_SIZE, RawCursorStream, build_raw_stream};
use crate::{DbError, DjogiError};
use postgres_types::{FromSql, ToSql};
use tokio_postgres::Row;

mod sealed {
    pub trait Sealed {}

    impl Sealed for crate::context::DjogiContext {}
    impl Sealed for crate::pg::pool::DjogiPool {}
}

/// Sealed extension trait exposing djogi's raw SQL context escape hatches.
///
/// Base trait: no `Send` bound. The generated [`RawAccessExt`] variant adds
/// `Send` bounds to the futures returned by async methods.
#[doc(hidden)]
#[trait_variant::make(RawAccessExt: Send)]
pub trait RawAccessExtBase: sealed::Sealed {
    async fn raw_query<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError>;

    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError>;

    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>;

    async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: for<'row> FromSql<'row> + Send + 'static;

    async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError>;

    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError>;

    async fn raw_stream<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'ctx>, DjogiError>;

    async fn raw_stream_with_fetch_size<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        fetch_size: u32,
    ) -> Result<RawCursorStream<'ctx>, DjogiError>;
}

impl RawAccessExt for DjogiContext {
    async fn raw_query<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError> {
        let rows = self.__query_all_for_macros(sql, params).await?;
        rows.iter().map(T::from_pg_row).collect()
    }

    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        self.__query_all_for_macros(sql, params).await
    }

    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError> {
        let row = self
            .__query_opt_for_macros(sql, params)
            .await?
            .ok_or_else(|| DjogiError::not_found("<raw>"))?;
        T::from_pg_row(&row)
    }

    async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: for<'row> FromSql<'row> + Send + 'static,
    {
        let row = self
            .__query_opt_for_macros(sql, params)
            .await?
            .ok_or_else(|| DjogiError::not_found("<raw>"))?;
        try_get_scalar(&row, 0)
    }

    async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        self.__execute_for_macros(sql, params).await
    }

    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError> {
        self.batch_execute(sql).await
    }

    async fn raw_stream<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'ctx>, DjogiError> {
        build_raw_stream(self, sql, params, DEFAULT_FETCH_SIZE).await
    }

    async fn raw_stream_with_fetch_size<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        fetch_size: u32,
    ) -> Result<RawCursorStream<'ctx>, DjogiError> {
        if fetch_size == 0 {
            return Err(DjogiError::Validation(
                "raw_stream fetch_size must be at least 1".to_owned(),
            ));
        }
        build_raw_stream(self, sql, params, fetch_size).await
    }
}

/// Sealed extension trait exposing pool/client escape hatches.
///
/// Base trait: no `Send` bound. The generated [`RawPoolAccessExt`] variant
/// adds `Send` bounds to the future returned by `raw_with_client`.
#[doc(hidden)]
#[trait_variant::make(RawPoolAccessExt: Send)]
pub trait RawPoolAccessExtBase: sealed::Sealed {
    fn raw_pool(&self) -> Option<&DjogiPool>;

    fn raw_conn(&mut self) -> Option<&mut PgConnection>;

    async fn raw_with_client<F, R>(&self, f: F) -> Result<R, DjogiError>
    where
        F: for<'client> FnOnce(&'client mut tokio_postgres::Client) -> ClientFuture<'client, R>
            + Send,
        R: Send + 'static;
}

impl RawPoolAccessExt for DjogiContext {
    fn raw_pool(&self) -> Option<&DjogiPool> {
        self.pool()
    }

    fn raw_conn(&mut self) -> Option<&mut PgConnection> {
        self.conn()
    }

    fn raw_with_client<F, R>(
        &self,
        f: F,
    ) -> impl std::future::Future<Output = Result<R, DjogiError>> + Send
    where
        F: for<'client> FnOnce(&'client mut tokio_postgres::Client) -> ClientFuture<'client, R>
            + Send,
        R: Send + 'static,
    {
        let pool = self.pool().cloned();
        async move {
            match pool {
                Some(pool) => pool.with_client(f).await,
                None => Err(DjogiError::Db(DbError::other(
                    "raw_with_client requires a pool-backed DjogiContext",
                ))),
            }
        }
    }
}

impl RawPoolAccessExt for DjogiPool {
    fn raw_pool(&self) -> Option<&DjogiPool> {
        Some(self)
    }

    fn raw_conn(&mut self) -> Option<&mut PgConnection> {
        None
    }

    fn raw_with_client<F, R>(
        &self,
        f: F,
    ) -> impl std::future::Future<Output = Result<R, DjogiError>> + Send
    where
        F: for<'client> FnOnce(&'client mut tokio_postgres::Client) -> ClientFuture<'client, R>
            + Send,
        R: Send + 'static,
    {
        let pool = self.clone();
        async move { pool.with_client(f).await }
    }
}

#[cfg(test)]
#[allow(dead_code)]
async fn _raw_stream_trait_canary<'ctx>(
    ctx: &'ctx mut DjogiContext,
) -> Result<RawCursorStream<'ctx>, DjogiError> {
    let params: &[&(dyn ToSql + Sync)] = &[];
    <DjogiContext as RawAccessExt>::raw_stream(ctx, "SELECT 1", params).await
}

#[cfg(test)]
#[allow(dead_code)]
async fn _raw_stream_with_fetch_size_trait_canary<'ctx>(
    ctx: &'ctx mut DjogiContext,
) -> Result<RawCursorStream<'ctx>, DjogiError> {
    let params: &[&(dyn ToSql + Sync)] = &[];
    <DjogiContext as RawAccessExt>::raw_stream_with_fetch_size(ctx, "SELECT 1", params, 1).await
}
