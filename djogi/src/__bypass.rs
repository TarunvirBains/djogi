//! Deliberate raw SQL escape hatches — djogi's `unsafe`-equivalent.
//!
//! Raw SQL in djogi is treated culturally the way `unsafe` is in Rust: not
//! banned, but always conscious. This module is public so adopter crates and
//! workspace examples can opt in consciously, but the module itself is
//! `#[doc(hidden)]` (declared at the crate root) and the traits inside are
//! sealed. The supported way to bring these traits into scope is the
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
//! ## Post-query decode covered by the guard
//!
//! Raw SQL that succeeds server-side but produces a row the framework
//! cannot decode (e.g. `raw_scalar::<i32>("SELECT
//! set_config('application_name','poisoned',false)")` — the SQL ran,
//! the session GUC mutated, and `try_get_scalar` then fails because
//! the returned text is not an `i32`) is itself a dirty exit.
//! `raw_query`, `raw_fetch_one`, and `raw_scalar` route through
//! [`DjogiContext::query_all_with`](crate::context::DjogiContext) /
//! [`query_opt_with`](crate::context::DjogiContext) so the `FromPgRow` /
//! `try_get_scalar` decode runs **inside** the `PoolConnGuard`'s
//! lifetime. A decode failure flips the guard's `Result` to `Err`, so
//! `Drop` detaches the connection. `raw_execute`, `raw_ddl`, and
//! `raw_rows` have no post-query decode step — their existing pool
//! guard already covers the only Err/cancel exit shapes.
//!
//! ## Adopter contract
//!
//! Even with the dirty-by-default guard, raw SQL that mutates session
//! state (`SET ROLE`, `SET search_path`, session-scoped
//! `pg_advisory_lock`, `LISTEN`/`UNLISTEN`, prepared-statement creation
//! outside the cache, etc.) on the **clean-exit path** still leaves the
//! connection in a non-default state when it returns to the pool. The
//! dirty-by-default guard fires on `Err`, panic, and future cancellation
//! only — not on `Ok`.
//!
//! For session-state-affecting raw SQL, wrap the call in
//! [`crate::transaction::atomic`] **and** either:
//!
//! - use a TRANSACTION-LOCAL form so `COMMIT` or `ROLLBACK` clears the
//!   state automatically — `SET LOCAL key = value` instead of `SET key
//!   = value`, `set_config(name, value, true)` instead of
//!   `set_config(name, value, false)`, `pg_advisory_xact_lock(…)`
//!   instead of `pg_advisory_lock(…)`, etc.; or
//! - explicitly reset / unlock / `UNLISTEN` / `DEALLOCATE` the
//!   session-level mutation on **every non-cancel exit** of the closure
//!   — before returning `Ok`, in every error branch, and in any panic
//!   recovery. `atomic()` will NOT do this cleanup for you on Err/panic;
//!   see the next paragraph.
//!
//! **`atomic()` is a transaction guard, not a session-state reset
//! guard.** Its `ROLLBACK` path on Err/panic only unwinds
//! TRANSACTION-SCOPED state (row writes, sequence allocations, `SET
//! LOCAL`, `set_config(_, _, true)`, `pg_advisory_xact_lock`).
//! SESSION-scoped state survives both clean `COMMIT` and `ROLLBACK` —
//! session advisory locks explicitly ignore transaction rollback per
//! Postgres semantics, plain `SET` / `SET ROLE` / `SET search_path` are
//! reset by `ROLLBACK` only when the SAME transaction issued them, and
//! `LISTEN` / prepared statements bypass transactional rollback
//! entirely. A `SET search_path = 'audit'` inside an `atomic()` closure
//! that returns `Ok` commits but the new `search_path` survives
//! `COMMIT` and rides the connection back to the pool; a
//! `pg_advisory_lock(...)` acquired inside `atomic()` that subsequently
//! returns `Err` is NOT released by `ROLLBACK` and the lock leaks.
//! Adopters must choose transaction-local forms or run explicit reset
//! on every non-cancel exit for the contract to hold.
//!
//! **`atomic()` cancellation caveat.** `atomic()` issues `ROLLBACK` on
//! the closure's `Err` and panic paths. It does NOT issue `ROLLBACK`
//! when the entire `atomic()` future is dropped mid-execution (e.g.
//! `tokio::time::timeout(..., atomic(&pool, |tx| async { ... }))`
//! firing the timeout before the closure resolves). In that case the
//! transaction-backed `DjogiContext` drops without async cleanup and
//! the underlying connection returns to the pool with the transaction
//! still open. This is a pre-existing transaction-scope hazard tracked
//! separately from djogi#162; it is not introduced by the pool-path
//! guard this module describes, but adopters relying on `atomic()` as
//! a session-state isolation mechanism should avoid wrapping it in
//! cancellation primitives until that gap is closed.
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
//! # Reaching the raw API
//!
//! Adopter code reaches these traits only through the bypass attribute, not
//! through `use djogi::__bypass::*;` at the import site. The attribute brings
//! the trait methods into scope on the `DjogiContext` / `DjogiPool` value
//! inside the decorated item:
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! #[djogi::deliberately_bypass_convention_with_raw_sql]
//! // JUSTIFICATION (djogi#234): citext column needs case-insensitive
//! // equality; QuerySet doesn't expose LOWER(col) equality yet.
//! async fn count_users_ci(ctx: &mut DjogiContext, name: &str) -> djogi::Result<i64> {
//!     ctx.raw_scalar(
//!         "SELECT COUNT(*) FROM users WHERE LOWER(name) = LOWER($1)",
//!         &[&name],
//!     ).await
//! }
//! ```
//!
//! # Cross-references
//!
//! - **Specification** — [Raw SQL escape hatches](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md)
//!   for the full contract, the JUSTIFICATION convention, the pin-test
//!   carve-out, and the "no ergonomic raw SQL" decision.
//! - **Pool-level escape hatch** — see [`RawPoolAccessExtBase::raw_with_client`]
//!   when binary protocol, `COPY`, or `CREATE EXTENSION` requires bypassing
//!   the per-statement `tokio_postgres::Statement` cache.
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
//! ## Post-query decode covered by the guard
//!
//! Raw SQL that succeeds server-side but produces a row the framework
//! cannot decode (e.g. `raw_scalar::<i32>("SELECT
//! set_config('application_name','poisoned',false)")` — the SQL ran,
//! the session GUC mutated, and `try_get_scalar` then fails because
//! the returned text is not an `i32`) is itself a dirty exit.
//! `raw_query`, `raw_fetch_one`, and `raw_scalar` route through
//! [`DjogiContext::query_all_with`](crate::context::DjogiContext) /
//! [`query_opt_with`](crate::context::DjogiContext) so the `FromPgRow` /
//! `try_get_scalar` decode runs **inside** the `PoolConnGuard`'s
//! lifetime. A decode failure flips the guard's `Result` to `Err`, so
//! `Drop` detaches the connection. `raw_execute`, `raw_ddl`, and
//! `raw_rows` have no post-query decode step — their existing pool
//! guard already covers the only Err/cancel exit shapes.
//!
//! ## Adopter contract
//!
//! Even with the dirty-by-default guard, raw SQL that mutates session
//! state (`SET ROLE`, `SET search_path`, session-scoped
//! `pg_advisory_lock`, `LISTEN`/`UNLISTEN`, prepared-statement creation
//! outside the cache, etc.) on the **clean-exit path** still leaves the
//! connection in a non-default state when it returns to the pool. The
//! dirty-by-default guard fires on `Err`, panic, and future cancellation
//! only — not on `Ok`.
//!
//! For session-state-affecting raw SQL, wrap the call in
//! [`crate::transaction::atomic`] **and** either:
//!
//! - use a TRANSACTION-LOCAL form so `COMMIT` or `ROLLBACK` clears the
//!   state automatically — `SET LOCAL key = value` instead of `SET key
//!   = value`, `set_config(name, value, true)` instead of
//!   `set_config(name, value, false)`, `pg_advisory_xact_lock(…)`
//!   instead of `pg_advisory_lock(…)`, etc.; or
//! - explicitly reset / unlock / `UNLISTEN` / `DEALLOCATE` the
//!   session-level mutation on **every non-cancel exit** of the closure
//!   — before returning `Ok`, in every error branch, and in any panic
//!   recovery. `atomic()` will NOT do this cleanup for you on Err/panic;
//!   see the next paragraph.
//!
//! **`atomic()` is a transaction guard, not a session-state reset
//! guard.** Its `ROLLBACK` path on Err/panic only unwinds
//! TRANSACTION-SCOPED state (row writes, sequence allocations, `SET
//! LOCAL`, `set_config(_, _, true)`, `pg_advisory_xact_lock`).
//! SESSION-scoped state survives both clean `COMMIT` and `ROLLBACK` —
//! session advisory locks explicitly ignore transaction rollback per
//! Postgres semantics, plain `SET` / `SET ROLE` / `SET search_path` are
//! reset by `ROLLBACK` only when the SAME transaction issued them, and
//! `LISTEN` / prepared statements bypass transactional rollback
//! entirely. A `SET search_path = 'audit'` inside an `atomic()` closure
//! that returns `Ok` commits but the new `search_path` survives
//! `COMMIT` and rides the connection back to the pool; a
//! `pg_advisory_lock(...)` acquired inside `atomic()` that subsequently
//! returns `Err` is NOT released by `ROLLBACK` and the lock leaks.
//! Adopters must choose transaction-local forms or run explicit reset
//! on every non-cancel exit for the contract to hold.
//!
//! **`atomic()` cancellation caveat.** `atomic()` issues `ROLLBACK` on
//! the closure's `Err` and panic paths. It does NOT issue `ROLLBACK`
//! when the entire `atomic()` future is dropped mid-execution (e.g.
//! `tokio::time::timeout(..., atomic(&pool, |tx| async { ... }))`
//! firing the timeout before the closure resolves). In that case the
//! transaction-backed `DjogiContext` drops without async cleanup and
//! the underlying connection returns to the pool with the transaction
//! still open. This is a pre-existing transaction-scope hazard tracked
//! separately from djogi#162; it is not introduced by the pool-path
//! guard this module describes, but adopters relying on `atomic()` as
//! a session-state isolation mechanism should avoid wrapping it in
//! cancellation primitives until that gap is closed.
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
/// `Send` bounds to the futures returned by async methods. Reaching any
/// method here is djogi's `unsafe`-equivalent — see the
/// [module docs](self) and the
/// [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
#[doc(hidden)]
#[trait_variant::make(RawAccessExt: Send)]
pub trait RawAccessExtBase: sealed::Sealed {
    /// Run a raw `SELECT` and decode every row into `T` via
    /// [`FromPgRow`](crate::pg::decode::FromPgRow).
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Prefer the typed surface — `Model::objects().filter(...).fetch_all(ctx)`
    /// — for any predicate the queryset can express. Reach for `raw_query`
    /// only for shapes the typed layer cannot describe today (recursive CTEs,
    /// set-returning functions, bespoke joins).
    ///
    /// `T: FromPgRow` decodes positionally against the wire row, so the
    /// `SELECT` projection list must match the model's column order. The
    /// canonical order is `id, created_at, updated_at, ...user_fields` for
    /// `#[model]`-derived structs; ad-hoc rowtypes implement
    /// [`FromPgRow`](crate::pg::decode::FromPgRow) with whatever shape they
    /// need.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#234): typed surface lacks recursive CTE.
    /// async fn ancestor_threads(ctx: &mut DjogiContext, root_id: HeerId)
    ///     -> djogi::Result<Vec<Comment>>
    /// {
    ///     ctx.raw_query(
    ///         "WITH RECURSIVE ancestors AS (
    ///              SELECT * FROM comments WHERE id = $1
    ///              UNION ALL
    ///              SELECT c.* FROM comments c
    ///              JOIN ancestors a ON c.id = a.parent_id
    ///          )
    ///          SELECT id, created_at, updated_at, parent_id, body
    ///          FROM ancestors",
    ///         &[&root_id],
    ///     ).await
    /// }
    /// ```
    async fn raw_query<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError>;

    /// Run a raw `SELECT` and return undecoded `tokio_postgres::Row` values.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Prefer the typed surface — `Model::objects().filter(...).fetch_all(ctx)`
    /// — for any predicate the queryset can express. If the typed surface
    /// cannot describe the shape but the row decodes into a `FromPgRow`,
    /// prefer [`raw_query`](RawAccessExtBase::raw_query) over `raw_rows` so
    /// the per-row decode is positional and debug-asserted. Reach for
    /// `raw_rows` only when the caller really does need to inspect column
    /// metadata or call [`tokio_postgres::Row::try_get`] on heterogenous
    /// columns by name (e.g. dynamic introspection helpers).
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#456): introspecting column metadata for the admin
    /// // schema diff renderer; FromPgRow does not expose column types.
    /// async fn dump_columns(ctx: &mut DjogiContext, table: &str)
    ///     -> djogi::Result<Vec<tokio_postgres::Row>>
    /// {
    ///     ctx.raw_rows(
    ///         "SELECT column_name, data_type FROM information_schema.columns
    ///          WHERE table_name = $1",
    ///         &[&table],
    ///     ).await
    /// }
    /// ```
    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError>;

    /// Run a raw `SELECT` expected to return exactly one row, decoded into `T`.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Returns [`DjogiError::NotFound`] when the query produces zero rows;
    /// the framework does not enforce the upper bound, so the caller is
    /// responsible for using `LIMIT 1` (or otherwise guaranteeing
    /// uniqueness) when required. Prefer
    /// [`Model::get`](crate::model::Model::get) /
    /// `QuerySet::fetch_one` for typed-surface lookups.
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#789): typed surface lacks JSON-aggregated reads.
    /// async fn fetch_one_summary(ctx: &mut DjogiContext, id: HeerId)
    ///     -> djogi::Result<UserSummary>
    /// {
    ///     ctx.raw_fetch_one(
    ///         "SELECT id, jsonb_build_object('posts', count(p.id)) AS summary
    ///          FROM users u LEFT JOIN posts p ON p.author_id = u.id
    ///          WHERE u.id = $1 GROUP BY u.id LIMIT 1",
    ///         &[&id],
    ///     ).await
    /// }
    /// ```
    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>;

    /// Run a raw `SELECT` and return the first column of the first row as a
    /// scalar value of type `T`.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Returns [`DjogiError::NotFound`] when the query produces zero rows.
    /// Use for `SELECT COUNT(*)`, `SELECT MAX(...)`, and similar single-value
    /// reductions. Prefer the queryset's `.count(ctx)` / `.exists(ctx)` /
    /// aggregate-projection terminals when the typed surface covers the
    /// shape.
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#101): summary aggregate the visage layer doesn't
    /// // yet project for nested JSONB facets.
    /// async fn open_invoices_total_cents(ctx: &mut DjogiContext)
    ///     -> djogi::Result<i64>
    /// {
    ///     ctx.raw_scalar(
    ///         "SELECT COALESCE(SUM(total_cents), 0)
    ///          FROM invoices WHERE status = 'open'",
    ///         &[],
    ///     ).await
    /// }
    /// ```
    async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: for<'row> FromSql<'row> + Send + 'static;

    /// Run a raw `INSERT`, `UPDATE`, `DELETE`, or other no-row-returning
    /// statement and return the affected-row count.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Prefer `Model::create` / `Model::save` / `Model::delete` for single-row
    /// CRUD and `QuerySet::update` / `QuerySet::delete` for bulk writes. Reach
    /// for `raw_execute` only for shapes the typed layer cannot express today
    /// (e.g. preserving `updated_at` across a bulk update — the queryset
    /// always stamps it).
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#202): bulk update must preserve updated_at; the
    /// // queryset bulk-update path always stamps `updated_at = now()`.
    /// async fn restamp_recent(ctx: &mut DjogiContext, days: i32)
    ///     -> djogi::Result<u64>
    /// {
    ///     ctx.raw_execute(
    ///         "UPDATE posts SET view_count = view_count + 1
    ///          WHERE created_at > now() - $1::interval",
    ///         &[&format!("{days} days")],
    ///     ).await
    /// }
    /// ```
    async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError>;

    /// Run a raw DDL batch (one or more semicolon-separated statements,
    /// no parameters).
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// `raw_ddl` is `batch_execute(sql)` under a friendlier name — it
    /// carries the same blast radius as [`raw_execute`](RawAccessExtBase::raw_execute)
    /// and intentionally does not project through the migration substrate.
    /// Tests that need to set up tables MUST use
    /// `#[djogi::djogi_test(sync_models = [...])]` instead — `sync_models`
    /// projects through the descriptor / `pk_default_sql` pipeline so
    /// projection bugs surface from the test surface (tracking issue
    /// djogi#133).
    ///
    /// Reach for `raw_ddl` only for setup that cannot live in a model
    /// descriptor (`CREATE EXTENSION`, custom types declared outside djogi's
    /// schema-snapshot model, role / permission grants).
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#303): PostGIS extension install runs once per
    /// // database; the future #[djogi_test(extensions = ["postgis"])] surface
    /// // (Phase 6.5) is the preferred path.
    /// async fn install_postgis(ctx: &mut DjogiContext) -> djogi::Result<()> {
    ///     ctx.raw_ddl("CREATE EXTENSION IF NOT EXISTS postgis").await
    /// }
    /// ```
    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError>;

    /// Open a server-side cursor and yield rows lazily as a
    /// [`RawCursorStream`].
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Postgres cursors are transaction-local — the surrounding context MUST
    /// be transaction-backed. Calling `raw_stream` on a pool-backed context
    /// returns [`DjogiError::StreamOutsideTransaction`] at construction time
    /// (not at the first `poll_next`), so the misuse surfaces immediately.
    /// Wrap the consumer in `atomic(&mut ctx, |tx| Box::pin(async move {
    /// ... }))` so the `tx` argument is transaction-backed.
    ///
    /// Uses the framework default fetch size (chunk-size for the
    /// `FETCH FORWARD` calls under the cursor). For control over the chunk
    /// shape, use [`raw_stream_with_fetch_size`](RawAccessExtBase::raw_stream_with_fetch_size).
    /// Prefer `QuerySet::stream(ctx)` for typed-surface streaming.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    /// use futures::StreamExt;
    ///
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#404): export job needs server-side cursor; the
    /// // typed QuerySet::stream is preferred when the shape fits.
    /// async fn export_orders(ctx: &mut DjogiContext) -> djogi::Result<()> {
    ///     atomic(ctx, |tx| Box::pin(async move {
    ///         let mut stream = tx.raw_stream(
    ///             "SELECT id, total_cents FROM orders ORDER BY id",
    ///             &[],
    ///         ).await?;
    ///         while let Some(row) = stream.next().await {
    ///             let _row = row?; // process row
    ///         }
    ///         Ok(())
    ///     })).await
    /// }
    /// ```
    async fn raw_stream<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'ctx>, DjogiError>;

    /// Same as [`raw_stream`](RawAccessExtBase::raw_stream) but caller-tunable
    /// `FETCH FORWARD` chunk size.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Prefer the typed surface — `Model::objects()` / `QuerySet::stream`
    /// inside an `atomic(...)` scope — for any shape the typed layer can
    /// describe. Reach for `raw_stream_with_fetch_size` only when the typed
    /// stream cannot describe the projection AND the default chunk size used
    /// by `raw_stream` is the wrong shape for the consumer (typically very
    /// large exports or very latency-sensitive previews).
    ///
    /// `fetch_size` of `0` returns [`DjogiError::Validation`] — the cursor
    /// driver cannot make progress on an empty fetch chunk. Larger values
    /// reduce round-trips at the cost of per-chunk memory; smaller values
    /// reduce latency to the first row at the cost of more network round
    /// trips. The framework default (used by `raw_stream`) is a balanced
    /// middle ground.
    ///
    /// Same transaction-context rules as [`raw_stream`](RawAccessExtBase::raw_stream):
    /// pool-backed contexts return [`DjogiError::StreamOutsideTransaction`]
    /// at construction time.
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#505): export job tunes chunk size to match the
    /// // downstream consumer's batch boundary.
    /// async fn export_orders_chunked(ctx: &mut DjogiContext) -> djogi::Result<()> {
    ///     atomic(ctx, |tx| Box::pin(async move {
    ///         let mut stream = tx.raw_stream_with_fetch_size(
    ///             "SELECT id, total_cents FROM orders ORDER BY id",
    ///             &[],
    ///             100, // fetch 100 rows per round-trip
    ///         ).await?;
    ///         use futures::StreamExt;
    ///         while let Some(row) = stream.next().await {
    ///             let _row = row?;
    ///         }
    ///         Ok(())
    ///     })).await
    /// }
    /// ```
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
        // Route through `query_all_with` so the per-row `FromPgRow::from_pg_row`
        // decode runs inside the `PoolConnGuard`'s lifetime. A decode failure
        // here would otherwise leave the pool with a possibly poisoned
        // connection — the underlying SQL succeeded (guard armed for clean
        // return) while the framework-side decode failed afterwards.
        self.query_all_with(sql, params, |rows| {
            rows.iter().map(T::from_pg_row).collect()
        })
        .await
    }

    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        // No post-query decode — the existing `query_all` guard already
        // covers the only Err/cancel exit shape.
        self.__query_all_for_macros(sql, params).await
    }

    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError> {
        // Decode runs inside the guard's lifetime via `query_opt_with`. The
        // `not_found` branch is also reported as `Err`, so the guard's
        // `committed` flag stays `false` and a no-row response still
        // recycles the connection cleanly (no session state mutated — the
        // recycle path is appropriate). Server-side failure paths are
        // funnelled through the inner `query_opt` `Err`, and decode
        // failures funnel through the `T::from_pg_row(...)` return.
        self.query_opt_with(sql, params, |row_opt| {
            let row = row_opt.ok_or_else(|| DjogiError::not_found("<raw>"))?;
            T::from_pg_row(&row)
        })
        .await
    }

    async fn raw_scalar<T>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>
    where
        T: for<'row> FromSql<'row> + Send + 'static,
    {
        // `try_get_scalar` is the decode step that can fail on a row that
        // the underlying SQL produced successfully — e.g.
        // `SELECT set_config('application_name', '...', false)` returns
        // text and mutates the session GUC, so calling
        // `raw_scalar::<i32>` decode-fails AFTER the session was poisoned.
        // Routing through `query_opt_with` keeps that decode inside the
        // guard's lifetime so the connection detaches on the Err path.
        self.query_opt_with(sql, params, |row_opt| {
            let row = row_opt.ok_or_else(|| DjogiError::not_found("<raw>"))?;
            try_get_scalar(&row, 0)
        })
        .await
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
/// adds `Send` bounds to the future returned by `raw_with_client`. Reaching
/// any method here is djogi's `unsafe`-equivalent — see the
/// [module docs](self) and the
/// [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
#[doc(hidden)]
#[trait_variant::make(RawPoolAccessExt: Send)]
pub trait RawPoolAccessExtBase: sealed::Sealed {
    /// Borrow the underlying [`DjogiPool`] when the context is pool-backed.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Returns `None` when the context is transaction-backed — pool reads
    /// during a transaction would route around the surrounding scope. Use
    /// for pool-state introspection (capacity, idle counts) when wiring
    /// adopter-side metrics; otherwise prefer the typed surface
    /// (`DjogiContext::from_pool` for fresh handles, `share_pool` to clone
    /// the inner `Arc`).
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#606): pool-state introspection for adopter
    /// // metrics; the typed surface does not yet expose pool-stats reads.
    /// fn pool_status(ctx: &DjogiContext) -> Option<usize> {
    ///     ctx.raw_pool().map(|p| p.status().available)
    /// }
    /// ```
    fn raw_pool(&self) -> Option<&DjogiPool>;

    /// Borrow the underlying [`PgConnection`] when the context is
    /// transaction-backed.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Returns `None` when the context is pool-backed — there is no
    /// long-lived connection to borrow. Use for connection-state inspection
    /// (savepoint depth, in-progress transaction state) when an adopter-side
    /// helper needs to branch on the inner state. Prefer
    /// [`DjogiContext::savepoint_depth`](crate::DjogiContext::savepoint_depth)
    /// and the typed transaction substrate for ordinary use.
    ///
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#707): transaction-state inspection for a custom
    /// // tracing layer.
    /// fn debug_conn(ctx: &mut DjogiContext) -> bool {
    ///     ctx.raw_conn().is_some()
    /// }
    /// ```
    fn raw_conn(&mut self) -> Option<&mut PgConnection>;

    /// Run a closure with a checked-out raw [`tokio_postgres::Client`] from
    /// the underlying pool.
    ///
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    ///
    /// Prefer the typed surface — `Model::objects()` / `QuerySet`,
    /// `Model::create` / `save` / `delete`, and `djogi::transaction::atomic`
    /// — for routine reads, writes, and transactions. `raw_with_client` is
    /// the framework's only path to the underlying `tokio_postgres::Client`
    /// and the only way to reach binary-protocol helpers like
    /// `client.copy_in(...)`, `client.simple_query(...)`, `CREATE EXTENSION`
    /// (which requires `simple_query` outside a transaction), and the
    /// prepared-statement cache directly — typed-surface equivalents do not
    /// exist for those binary-protocol primitives today. The closure receives
    /// `&mut Client` for the duration of the borrow; the returned connection
    /// is **dirty by default** — adopters that issue `SET` / `LISTEN` / role
    /// changes inside the closure are responsible for resetting the
    /// connection (or the surrounding pool's `Manager` impl must declare a
    /// `reset` step).
    ///
    /// Returns [`DjogiError::Db`] wrapping the underlying transport / pool
    /// error when the context has no pool to draw from (pure transaction-
    /// scoped contexts cannot satisfy `raw_with_client`).
    ///
    /// See the [connection-pool guide](https://github.com/tarunvir/djogi/blob/main/docs/guide/pool.md#raw-client-escape-hatch--raw_with_client)
    /// for the canonical treatment of when to reach for this surface.
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#808): COPY IN ingest needs binary protocol; the
    /// // typed surface has no streaming-bulk-insert primitive yet.
    /// async fn copy_in_orders(pool: &DjogiPool) -> djogi::Result<()> {
    ///     pool.raw_with_client(|client| Box::pin(async move {
    ///         let _sink = client.copy_in("COPY orders FROM STDIN").await?;
    ///         // write rows to the sink ...
    ///         Ok(())
    ///     })).await
    /// }
    /// ```
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
