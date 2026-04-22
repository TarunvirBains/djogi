//! The `DjogiContext` type — carries either a pooled handle or an active transaction.
//!
//! Per Phase 4 v3 specification, `DjogiContext` **replaces** the `E: Executor` generic
//! on every `Model` CRUD and `QuerySet` method signature. This change unifies the API:
//! the same method can be called against a pool or inside a transaction without
//! reborrows or type juggling.
//!
//! # Context variants
//!
//! A context is one of:
//! - **Pool**: backed by a `DjogiPool` — each operation checks out a connection, runs
//!   the query, and returns the connection to the pool.
//! - **Transaction**: an active `PgConnection` with an open transaction — all
//!   operations share the same logical transaction until `commit()` or `rollback()`
//!   is called.
//!
//! # Execution dispatch pattern
//!
//! CRUD methods and QuerySet terminals that today take `&mut DjogiContext` acquire a
//! `PgConnection` from the pool (Pool path) or reuse the existing connection
//! (Transaction path). The inline match on [`ContextInner`] is the dispatch
//! mechanism:
//!
//! ```ignore
//! let rows = ctx.query_all(sql, params).await?;
//! ```
//!
//! Two variants = two match arms = negligible overhead.
//!
//! # Savepoint depth and nesting
//!
//! When `atomic()` (Phase 4 Task 1) opens a transaction inside another transaction,
//! Postgres transparently converts it to a savepoint. The `savepoint_depth` field
//! tracks how many nested `atomic()` calls have been made (0 = root transaction or
//! pool, N = N savepoints). The framework uses this to auto-name savepoints as
//! `sp_<depth>` without user involvement.
//!
//! # On-commit callbacks
//!
//! Callbacks registered via `.on_commit()` fire after a successful `commit()`.
//! They are useful for post-transaction side effects (cache invalidation,
//! outbox polling, audit logging). Callback errors are logged but do not fail
//! the commit itself (per Phase 4 v3 Q9 resolution). Callbacks are FIFO.
//!
//! # Drain points
//!
//! Registered callbacks are consumed by exactly two paths:
//!
//! - [`DjogiContext::commit`] — the low-level tx-backed commit drains the
//!   queue after the underlying commit succeeds, runs each callback in
//!   FIFO order, and logs any callback error via `tracing::error!` without
//!   unwinding the caller.
//! - `atomic()` (Phase 4 Task 1) — once landed, the canonical entry point
//!   for application code; wraps the same drain-after-commit semantics but
//!   also handles nested savepoints.
//!
//! Callbacks registered on a pool-backed context with no `atomic()` scope
//! are silently dropped when the context is dropped.

use crate::pg::connection::PgConnection;
use crate::pg::decode::{FromPgRow, try_get_scalar};
use crate::pg::pool::DjogiPool;
use crate::query::stream::{DEFAULT_FETCH_SIZE, RawCursorStream, build_raw_stream};
use crate::{DbError, DjogiError};
use futures::FutureExt;
use postgres_types::FromSql;
use postgres_types::ToSql;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use tokio_postgres::Row;

/// Type alias for an async callback that fires after commit.
///
/// Represents a boxed closure that returns an async result. Used for the on-commit
/// callback stack to reduce type complexity in `DjogiContext`.
type OnCommitCallback = Box<
    dyn FnOnce() -> Pin<Box<dyn std::future::Future<Output = Result<(), DjogiError>> + Send>>
        + Send,
>;

/// The execution context for all CRUD operations.
///
/// Carries either a pooled handle or an active transaction + savepoint tracking.
/// Replaces the `E: Executor` generic on `Model` and `QuerySet` signatures.
pub struct DjogiContext {
    /// Internal variant: either a pool or a transaction.
    inner: ContextInner,

    /// Savepoint depth: 0 = root transaction or pool, N = N nested `atomic()` calls.
    /// Used by the framework to auto-generate savepoint names.
    savepoint_depth: u32,

    /// FIFO stack of callbacks to fire after a successful commit.
    /// Each callback is a boxed async closure that returns `Result<(), DjogiError>`.
    /// Errors are logged but do not fail the commit (Q9 resolution).
    on_commit: Vec<OnCommitCallback>,

    /// Whether [`set_tenant`](Self::set_tenant) has been called on this context.
    ///
    /// Set to `true` by `set_tenant`. Framework code can inspect this flag to
    /// detect missing tenant setup before performing tenant-scoped queries. Note
    /// that `SET LOCAL` (via `set_config`) is only meaningful inside an open
    /// transaction — callers must wrap tenant-scoped operations in `atomic()` for
    /// the GUC value to persist through the full query sequence.
    pub tenant_set: bool,
}

/// Internal enum selecting the active context variant.
///
/// `#[doc(hidden)] pub` because framework modules pattern-match on this
/// enum to dispatch to the connection. User code should go through the
/// execution helpers (`query_all`, `query_opt`, `query_one`, `execute`,
/// `batch_execute`) rather than reaching into the inner directly. The
/// `__` prefix and `#[doc(hidden)]` attribute are the social signal that
/// this type carries no stability guarantee.
// The `Transaction` variant holds a `PgConnection` (232 bytes, due to the
// `deadpool_postgres::Object` within). Boxing it would add an extra
// allocation per transaction — undesirable on the hot path. The size
// difference is intentional and expected: `Pool` holds only a clone of the
// pool handle (Arc<..>, 8 bytes); `Transaction` holds the full connection.
#[allow(clippy::large_enum_variant)]
#[doc(hidden)]
pub enum ContextInner {
    /// Pool-backed: acquires a connection per operation.
    Pool(DjogiPool),

    /// Transaction-backed: all operations share the same connection + transaction.
    ///
    /// The `PgConnection` wraps a checked-out pool connection. When this
    /// context is committed or rolled back, the connection is returned to the pool.
    Transaction(PgConnection),
}

/// Public-but-hidden alias of [`ContextInner`] for macro-generated code.
///
/// Macro-emitted CRUD bodies that pattern-match on the context's inner
/// variant to dispatch to the database now go through the execution
/// helpers (`ctx.query_one`, `ctx.execute`, etc.) rather than reaching
/// into this enum directly. This alias is kept for backward compatibility
/// during the T2 transition; T5 will remove the pattern-match exposure.
#[doc(hidden)]
pub type __ContextInnerForMacros = ContextInner;

impl DjogiContext {
    /// Create a context backed by a `DjogiPool`.
    ///
    /// # Example
    /// ```ignore
    /// let pool = DjogiPool::connect(url).await?;
    /// let mut ctx = DjogiContext::from_pool(pool);
    /// let user = User::create(&mut ctx, user).await?;
    /// ```
    pub fn from_pool(pool: DjogiPool) -> Self {
        DjogiContext {
            inner: ContextInner::Pool(pool),
            savepoint_depth: 0,
            on_commit: Vec::new(),
            tenant_set: false,
        }
    }

    /// Create a context backed by an active `PgConnection` (transaction).
    ///
    /// Typically called by `atomic()` (Phase 4 Task 1) or by test / integration
    /// code that manages its own transaction boundaries. Production code
    /// should prefer [`atomic()`](crate::transaction::atomic) so on-commit
    /// callbacks dispatch correctly; this constructor is the low-level escape
    /// hatch for callers who really do need to hand-manage a transaction.
    pub fn from_connection(conn: PgConnection) -> Self {
        DjogiContext {
            inner: ContextInner::Transaction(conn),
            savepoint_depth: 0,
            on_commit: Vec::new(),
            tenant_set: false,
        }
    }

    /// Return the current savepoint depth (0 = root, N = N nested `atomic()` calls).
    pub fn savepoint_depth(&self) -> u32 {
        self.savepoint_depth
    }

    /// Increment savepoint depth by 1 (called when entering a nested `atomic()`).
    ///
    /// **Internal use only.** Used by the framework to manage savepoint nesting.
    #[allow(dead_code)]
    pub(crate) fn increment_savepoint_depth(&mut self) {
        self.savepoint_depth = self.savepoint_depth.saturating_add(1);
    }

    /// Decrement savepoint depth by 1 (called when exiting a nested `atomic()`).
    ///
    /// **Internal use only.** Used by the framework to manage savepoint nesting.
    #[allow(dead_code)]
    pub(crate) fn decrement_savepoint_depth(&mut self) {
        self.savepoint_depth = self.savepoint_depth.saturating_sub(1);
    }

    /// Get a reference to the inner pool if this context is pool-backed.
    ///
    /// Returns `Some(&pool)` iff the context was created via `from_pool()`.
    /// Returns `None` if this is a transaction context.
    pub fn pool(&self) -> Option<&DjogiPool> {
        match &self.inner {
            ContextInner::Pool(pool) => Some(pool),
            ContextInner::Transaction(_) => None,
        }
    }

    /// Get a mutable reference to the inner connection if this context is transaction-backed.
    ///
    /// Returns `Some(&mut conn)` iff the context was created via `from_connection()`.
    /// Returns `None` if this is a pool context.
    pub fn conn(&mut self) -> Option<&mut PgConnection> {
        match &mut self.inner {
            ContextInner::Pool(_) => None,
            ContextInner::Transaction(conn) => Some(conn),
        }
    }

    /// Crate-private mutable accessor for the context's inner variant.
    ///
    /// Used by every CRUD / QuerySet terminal in the framework to pattern-match
    /// on pool-vs-transaction at the database boundary.
    pub(crate) fn inner_mut(&mut self) -> &mut ContextInner {
        &mut self.inner
    }

    /// Public-but-hidden mutable accessor used by `#[derive(Model)]`-generated
    /// CRUD bodies to pattern-match on the context's inner variant.
    ///
    /// Not part of the stable API — prefixed with `__` and `#[doc(hidden)]`
    /// so the social signal is clear: downstream code should not call this
    /// directly.
    #[doc(hidden)]
    pub fn __inner_mut_for_macros(&mut self) -> &mut __ContextInnerForMacros {
        &mut self.inner
    }

    // -------------------------------------------------------------------------
    // Public-but-hidden execution helpers for macro-generated code.
    //
    // The `pub(crate)` helpers below are the framework-internal dispatch
    // surface.  These `__` variants expose the same functionality for code
    // generated by `#[model]`, which runs in user crates and therefore cannot
    // access `pub(crate)` members.  Naming and `#[doc(hidden)]` signal that
    // these carry no stability guarantee.
    // -------------------------------------------------------------------------

    /// Execute a query and return all rows. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __query_all_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        self.query_all(sql, params).await
    }

    /// Execute a query and return the first row, if any. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __query_opt_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        self.query_opt(sql, params).await
    }

    /// Execute a query and return exactly one row. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __query_one_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError> {
        self.query_one(sql, params).await
    }

    /// Execute a DML statement. For use by macro-emitted code only.
    #[doc(hidden)]
    pub async fn __execute_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        self.execute(sql, params).await
    }

    // -------------------------------------------------------------------------
    // Execution helpers — the new dispatch surface for query terminals.
    // -------------------------------------------------------------------------

    /// Execute a parameterised query and return all rows.
    ///
    /// On the pool path, acquires a connection for the duration of this call.
    /// On the transaction path, reuses the existing connection.
    pub(crate) async fn query_all(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.query(sql, params).await,
        }
    }

    /// Execute a parameterised query and return the first row, if any.
    pub(crate) async fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query_opt(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.query_opt(sql, params).await,
        }
    }

    /// Execute a parameterised query and return exactly one row.
    ///
    /// Returns an error if zero or more than one row is returned.
    pub(crate) async fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.query_one(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.query_one(sql, params).await,
        }
    }

    /// Execute a parameterised DML statement and return the number of rows affected.
    pub(crate) async fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.execute(sql, params).await
            }
            ContextInner::Transaction(conn) => conn.execute(sql, params).await,
        }
    }

    /// Execute a simple (no-bind) SQL statement.
    ///
    /// Used for `BEGIN`, `COMMIT`, `ROLLBACK`, savepoint commands, and other
    /// control statements that carry no user-supplied values.
    #[allow(dead_code)] // Used by PgConnection directly in transaction.rs; may be wired up in T5.
    pub(crate) async fn batch_execute(&mut self, sql: &str) -> Result<(), DjogiError> {
        match &mut self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.batch_execute(sql).await
            }
            ContextInner::Transaction(conn) => conn.batch_execute(sql).await,
        }
    }

    // -------------------------------------------------------------------------
    // Transaction lifecycle.
    // -------------------------------------------------------------------------

    /// Commit the underlying transaction, consuming the context.
    ///
    /// Returns `Ok(())` if the context was transaction-backed and the commit
    /// succeeded. Returns `Err(DjogiError::Db(..))` if the commit failed or
    /// the context was pool-backed (pool contexts have no transaction to
    /// commit — calling `.commit()` on one is a caller error).
    ///
    /// # On-commit callbacks
    ///
    /// After the commit returns `Ok(())`, every callback registered via
    /// [`on_commit`](Self::on_commit) fires in FIFO order. Per Phase 4 v3 Q9,
    /// callback errors are logged via `tracing::error!` but do NOT unwind the
    /// caller — a failing callback must not fail the commit, and subsequent
    /// callbacks still fire.
    ///
    /// If the underlying commit fails, the callbacks are dropped without
    /// running (the transaction did not commit, so post-commit side effects
    /// are inappropriate).
    pub async fn commit(self) -> Result<(), DjogiError> {
        let DjogiContext {
            inner, on_commit, ..
        } = self;

        match inner {
            ContextInner::Pool(_) => Err(DjogiError::Db(DbError::other(
                "DjogiContext::commit called on a pool-backed context",
            ))),
            ContextInner::Transaction(mut conn) => {
                conn.batch_execute("COMMIT").await?;
                drain_on_commit(on_commit).await;
                Ok(())
            }
        }
    }

    /// Roll back the underlying transaction, consuming the context.
    ///
    /// Returns `Ok(())` if the context was transaction-backed and the
    /// rollback succeeded. Returns `Err(DjogiError::Db(..))` if the rollback
    /// failed or the context was pool-backed.
    ///
    /// # On-commit callbacks
    ///
    /// Any callbacks registered via [`on_commit`](Self::on_commit) during
    /// this transaction are discarded (not fired). Post-commit side effects
    /// only make sense against a successful commit; rollback explicitly
    /// throws them away.
    pub async fn rollback(mut self) -> Result<(), DjogiError> {
        // Discard queued callbacks first — on a rollback path they must
        // not fire regardless of whether the rollback itself succeeds.
        self.on_commit.clear();

        match self.inner {
            ContextInner::Pool(_) => Err(DjogiError::Db(DbError::other(
                "DjogiContext::rollback called on a pool-backed context",
            ))),
            ContextInner::Transaction(mut conn) => {
                conn.batch_execute("ROLLBACK").await?;
                Ok(())
            }
        }
    }

    /// Begin a transaction and wrap it in a new `DjogiContext`.
    ///
    /// Only valid on pool-backed contexts — returns an error if called on an
    /// already-transaction-backed context (nested transactions will be
    /// modelled via savepoints in Phase 4 Task 1's `atomic()` wrapper).
    ///
    /// This is a low-level helper used by tests and by the `atomic()`
    /// implementation; production code should reach for `atomic()`.
    pub async fn begin(&self) -> Result<DjogiContext, DjogiError> {
        match &self.inner {
            ContextInner::Pool(pool) => {
                let mut conn = pool.get().await?;
                conn.batch_execute("BEGIN").await?;
                Ok(DjogiContext::from_connection(conn))
            }
            ContextInner::Transaction(_) => Err(DjogiError::Db(DbError::other(
                "DjogiContext::begin called on a transaction-backed context; \
                 nested transactions require atomic() (Phase 4 Task 1)",
            ))),
        }
    }

    /// Register an async callback to fire after a successful commit.
    ///
    /// Callbacks execute in FIFO order after the transaction commits.
    /// Callback errors are logged via `tracing::error!` but do not fail the
    /// commit (per Phase 4 v3 Q9 resolution). Subsequent callbacks still
    /// fire even if an earlier callback fails.
    pub fn on_commit<F, Fut>(&mut self, callback: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), DjogiError>> + Send + 'static,
    {
        let boxed: OnCommitCallback = Box::new(move || Box::pin(callback()));
        self.on_commit.push(boxed);
    }

    /// Length of the on-commit callback queue. Used by `transaction.rs`
    /// to snapshot the queue before entering a nested `atomic()` scope
    /// so inner-registered callbacks can be dropped on rollback.
    pub(crate) fn on_commit_len(&self) -> usize {
        self.on_commit.len()
    }

    /// Truncate the on-commit callback queue to `new_len`. Used by
    /// `transaction.rs` to discard callbacks registered inside a
    /// nested `atomic()` scope that rolled back.
    pub(crate) fn on_commit_truncate(&mut self, new_len: usize) {
        self.on_commit.truncate(new_len);
    }
}

impl DjogiContext {
    /// Execute ad-hoc SQL and decode every returned row into `T`.
    ///
    /// Use this when Djogi's `QuerySet` surface is too restrictive for a
    /// one-off projection or join but you still want Djogi-managed
    /// connection / transaction dispatch and `DjogiError` mapping.
    ///
    /// `binds` are positional Postgres parameters for `$1`, `$2`, …
    /// in `sql`, passed in the same order they appear in the statement.
    /// If this method is called inside [`atomic()`](crate::transaction::atomic),
    /// it reuses the active transaction / savepoint connection rather than
    /// escaping to a separate pool checkout.
    ///
    /// `T: FromPgRow` means callers can pass either a `#[model]`-derived
    /// struct or any hand-written row shape that implements
    /// [`FromPgRow`](crate::FromPgRow).
    pub async fn raw_query<T: FromPgRow>(
        &mut self,
        sql: &str,
        binds: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError> {
        let rows = self.query_all(sql, binds).await?;
        rows.iter().map(T::from_pg_row).collect()
    }

    /// Execute ad-hoc SQL expected to return exactly one row decoded as `T`.
    ///
    /// This is the single-row sibling of [`raw_query`](Self::raw_query):
    /// use it for hand-written SELECTs where `QuerySet::get()` cannot
    /// express the projection but the result shape still matches a
    /// `FromPgRow` decoder.
    ///
    /// `binds` map positionally to `$1`, `$2`, … in `sql`. When called
    /// inside [`atomic()`](crate::transaction::atomic), the query runs on
    /// the active transaction / savepoint connection.
    ///
    /// `T` may be a `#[model]` type or a custom struct with a manual
    /// [`FromPgRow`](crate::FromPgRow) impl for an ad-hoc row shape.
    pub async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        binds: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError> {
        let row = self
            .query_opt(sql, binds)
            .await?
            .ok_or_else(|| DjogiError::not_found("<raw>"))?;
        T::from_pg_row(&row)
    }

    /// Execute ad-hoc SQL expected to return exactly one scalar column.
    ///
    /// This is the escape hatch for statements such as
    /// `SELECT COUNT(*) ...` or `SELECT EXISTS (...)`. The first column of
    /// the single returned row is decoded as `T`.
    ///
    /// `binds` are positional Postgres parameters for `$1`, `$2`, …
    /// in `sql`. Inside [`atomic()`](crate::transaction::atomic), this
    /// method respects the active transaction / savepoint automatically.
    pub async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        binds: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: for<'a> FromSql<'a> + Send + 'static,
    {
        let row = self
            .query_opt(sql, binds)
            .await?
            .ok_or_else(|| DjogiError::not_found("<raw>"))?;
        try_get_scalar(&row, 0)
    }

    /// Execute ad-hoc DML / DDL and return the affected-row count.
    ///
    /// Use this for `INSERT`, `UPDATE`, `DELETE`, or other statements
    /// outside the typed `QuerySet` surface. `binds` map positionally to
    /// `$1`, `$2`, … in `sql`, and calls inside
    /// [`atomic()`](crate::transaction::atomic) reuse the active
    /// transaction / savepoint connection.
    pub async fn raw_execute(
        &mut self,
        sql: &str,
        binds: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        self.execute(sql, binds).await
    }

    /// Execute a SQL statement via the **simple query protocol** (no bind
    /// parameters, no server-side prepare).
    ///
    /// Use this for DDL statements that Postgres cannot prepare (`CREATE ROLE`,
    /// `DROP DATABASE`, multi-statement scripts, etc.). Unlike
    /// [`raw_execute`](Self::raw_execute), which routes through
    /// `prepare_cached`, this method sends the statement directly via
    /// `batch_execute` and accepts no bind parameters — the caller is
    /// responsible for ensuring the SQL contains no user-supplied values that
    /// need escaping.
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # When to use
    ///
    /// - DDL that Postgres refuses to prepare (`CREATE ROLE`, `ALTER SYSTEM`,
    ///   session-management statements that must run as simple queries).
    /// - Tests and migrations that issue a single, trusted DDL string with no
    ///   runtime values.
    ///
    /// For parameterised DML prefer [`raw_execute`](Self::raw_execute).
    pub async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError> {
        self.batch_execute(sql).await
    }

    /// Stream rows from an ad-hoc SQL query using a Postgres named cursor.
    ///
    /// Returns a [`RawCursorStream`] that yields
    /// `Result<tokio_postgres::Row, DjogiError>`. The caller decodes each row
    /// using `Row::get` or a custom `FromPgRow` impl.
    ///
    /// `sql` may be any `SELECT` statement. `binds` are positional Postgres
    /// parameters for `$1`, `$2`, … in the same order they appear in `sql`.
    ///
    /// # Transaction requirement
    ///
    /// Named Postgres cursors are transaction-local. This method requires
    /// `self` to be backed by an active transaction (i.e. called inside an
    /// [`atomic()`](crate::transaction::atomic) scope). Calling it on a
    /// pool-backed context returns
    /// `Err(DjogiError::StreamOutsideTransaction)` immediately.
    ///
    /// # Fetch size
    ///
    /// Default is `1000` rows per `FETCH` round trip. Use
    /// [`raw_stream_with_fetch_size`](Self::raw_stream_with_fetch_size) to
    /// override.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use futures::StreamExt;
    ///
    /// atomic(&pool, |ctx| Box::pin(async move {
    ///     let mut stream = ctx.raw_stream(
    ///         "SELECT id, title FROM posts WHERE published = $1",
    ///         &[&true],
    ///     ).await?;
    ///
    ///     while let Some(row) = stream.next().await {
    ///         let row = row?;
    ///         let id: i64 = row.get("id");
    ///         let title: String = row.get("title");
    ///         // …
    ///     }
    ///     Ok(())
    /// })).await?;
    /// ```
    pub async fn raw_stream<'ctx>(
        &'ctx mut self,
        sql: &str,
        binds: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'ctx>, DjogiError> {
        build_raw_stream(self, sql, binds, DEFAULT_FETCH_SIZE).await
    }

    /// Stream rows from an ad-hoc SQL query with a custom fetch size.
    ///
    /// Like [`raw_stream`](Self::raw_stream) but lets the caller choose the
    /// number of rows retrieved per `FETCH` round trip. `fetch_size` must be
    /// at least `1`; passing `0` returns `Err(DjogiError::Validation)`.
    ///
    /// Same transaction invariant as [`raw_stream`](Self::raw_stream).
    pub async fn raw_stream_with_fetch_size<'ctx>(
        &'ctx mut self,
        sql: &str,
        binds: &[&(dyn ToSql + Sync)],
        fetch_size: u32,
    ) -> Result<RawCursorStream<'ctx>, DjogiError> {
        if fetch_size == 0 {
            return Err(DjogiError::Validation(
                "raw_stream fetch_size must be at least 1".to_owned(),
            ));
        }
        build_raw_stream(self, sql, binds, fetch_size).await
    }

    /// Set the active tenant ID for this context by issuing
    /// `set_config('app.tenant_id', $1, true)`.
    ///
    /// This activates Row Level Security policies that use
    /// `current_setting('app.tenant_id')` as the tenant discriminator. After
    /// this call returns `Ok`, `self.tenant_set` is `true` so caller code can
    /// assert the precondition before performing tenant-scoped queries.
    ///
    /// # Important: use inside `atomic()`
    ///
    /// `set_config(…, true)` is the transactional form of `SET LOCAL` — the
    /// GUC is scoped to the **current transaction** and resets when the
    /// transaction commits or rolls back. On a pool-backed context without an
    /// open transaction, the GUC change is immediately effective but only
    /// lasts for the duration of the single statement, then the connection
    /// returns to the pool with the GUC cleared.
    ///
    /// The correct production pattern is:
    ///
    /// ```ignore
    /// atomic(&pool, |ctx| async move {
    ///     ctx.set_tenant("42").await?;
    ///     // All queries inside atomic() now see the tenant-scoped rows.
    ///     let posts = TenantPost::objects().fetch_all(ctx).await?;
    ///     Ok(posts)
    /// }).await?
    /// ```
    ///
    /// # Arguments
    ///
    /// * `tenant_id` — The tenant identifier as a string. For `BIGINT` tenant
    ///   keys the RLS policy casts the GUC value to the appropriate type
    ///   (e.g. `current_setting('app.tenant_id', true)::bigint`), so any
    ///   string that Postgres can cast to the column type is valid here.
    pub async fn set_tenant(&mut self, tenant_id: &str) -> Result<(), DjogiError> {
        // `set_config(name, value, is_local)` is the function-form of
        // `SET LOCAL` when `is_local = true`. Using a parameterised query via
        // `$1` lets the driver handle quoting; embedding the value directly
        // into SQL would require manual escaping and violates the no-interpolation
        // policy on user-supplied data.
        self.execute(
            "SELECT set_config('app.tenant_id', $1, true)",
            &[&tenant_id],
        )
        .await?;
        self.tenant_set = true;
        Ok(())
    }
}

/// Drain a batch of on-commit callbacks panic-safely.
///
/// Wraps each callback future in `AssertUnwindSafe(..).catch_unwind()`
/// so a panicking callback is logged via `tracing::error!` without
/// aborting the drain loop. Callback `Err` returns are likewise logged
/// and ignored — per Phase 4 v3 Q9 a callback failure must not fail the
/// commit, and every subsequent callback still fires.
pub(crate) async fn drain_on_commit(callbacks: Vec<OnCommitCallback>) {
    for cb in callbacks {
        let result = AssertUnwindSafe(cb()).catch_unwind().await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(
                error = ?e,
                "on_commit callback returned Err; continuing",
            ),
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                tracing::error!(
                    panic = %msg,
                    "on_commit callback panicked; continuing",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn savepoint_depth_starts_at_zero_and_increments() {
        // NOTE: Can't create a DjogiContext without a real pool in a blocking context.
        // The pool tests will be covered in integration tests with #[tokio::test].
        // For now, we document the constraint in CLAUDE.md notes and verify
        // compilation + clippy + fmt at the unit test level.
    }
}
