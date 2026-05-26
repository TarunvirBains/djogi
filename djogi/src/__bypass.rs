//! Deliberate raw SQL escape hatches — djogi's `unsafe`-equivalent.
//!
//! Raw SQL in djogi is not banned, but it is intentionally loud. This module
//! is public only so adopters, pin tests, and sibling workspace crates can opt
//! in consciously; it is hidden from rustdoc and its traits are sealed. The
//! supported unlock is `#[djogi::deliberately_bypass_convention_with_raw_sql]`
//! plus an adjacent `// JUSTIFICATION ...` comment. Under `tests/`, `cargo
//! xtask check-justifications` enforces that convention.
//!
//! Adopter code reaches these methods through the bypass attribute, not by
//! importing `djogi::__bypass::*` directly:
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! #[djogi::deliberately_bypass_convention_with_raw_sql]
//! // JUSTIFICATION (djogi#234): typed surface lacks recursive CTE support.
//! async fn count_users_ci(ctx: &mut DjogiContext, name: &str) -> djogi::Result<i64> {
//!     ctx.raw_scalar(
//!         "SELECT COUNT(*) FROM users WHERE LOWER(name) = LOWER($1)",
//!         &[&name],
//!     ).await
//! }
//! ```
//!
//! # Pool-backed lifecycle
//!
//! Pool-backed raw methods (`raw_query`, `raw_rows`, `raw_fetch_one`,
//! `raw_scalar`, `raw_execute`, `raw_ddl`) route through the same
//! dirty-by-default pool guards as
//! [`crate::pg::pool::DjogiPool::with_client`]:
//!
//! - `Ok` returns the connection to the pool normally.
//! - `Err`, panic, future cancellation, and post-query decode failure detach
//!   the connection instead of recycling it.
//!
//! This is required because Djogi uses
//! `deadpool_postgres::RecyclingMethod::Fast`, which only checks
//! `is_closed()` on return; it does **not** issue `ROLLBACK`, `RESET ALL`, or
//! `DISCARD ALL`. The extra detach cost on dirty exits is what prevents a
//! poisoned session from leaking to the next checkout.
//!
//! Even with that guard, a session-state mutation that returns `Ok` on a
//! pool-backed context still leaves the session non-default on the clean path.
//! If the caller deliberately runs session-scoped raw SQL outside a
//! transaction, they still own the cleanup contract.
//!
//! # Transaction-backed contract
//!
//! When the context is already inside [`crate::transaction::atomic`], djogi no
//! longer treats session-scoped raw SQL as "caller cleans it up later". The
//! bypass layer preflights transaction-backed `raw_query`, `raw_rows`,
//! `raw_fetch_one`, `raw_scalar`, `raw_execute`, and `raw_ddl` and rejects
//! these statement heads with
//! [`crate::DjogiError::SessionStatementDisallowedInTransaction`] before SQL
//! reaches Postgres:
//!
//! - plain `SET`
//! - `RESET`
//! - `DISCARD`
//! - `LISTEN`
//! - `UNLISTEN`
//! - `PREPARE`
//! - `DEALLOCATE`
//!
//! Transaction-local forms remain allowed: `SET LOCAL ...`,
//! `SET CONSTRAINTS ...`, and `SET TRANSACTION ...`.
//!
//! The refusal is intentionally conservative:
//!
//! - it applies only to transaction-backed raw entrypoints
//! - empty/trivia-only SQL passes through unchanged
//! - `raw_ddl` scans real top-level statements, respecting line comments,
//!   block comments, quoted strings, and dollar-quoted bodies
//! - `raw_stream` / `raw_stream_with_fetch_size` keep their existing
//!   transaction-required contract and do not run this classifier
//!
//! # Cancellation and poison
//!
//! Top-level `atomic()` cancellation no longer recycles an open transaction:
//! the dirty-drop guard detaches the connection on cancellation before it can
//! leak back to the pool.
//!
//! Nested `atomic()` cancellation remains fail-closed. If a nested future is
//! dropped before savepoint cleanup runs, the outer transaction is poisoned,
//! framework-owned work rejects further use, and `commit` rolls back instead
//! of committing. `raw_conn()` therefore returns `None` both for pool-backed
//! contexts and for poisoned transaction-backed contexts.
//!
//! Cursors, `COPY`, binary-protocol helpers, and other multi-round-trip driver
//! work should go through [`RawPoolAccessExt::raw_with_client`], which bounds
//! the protocol exchange to one checkout and applies the same dirty-detach
//! policy on dirty exit.
//!
//! Cross-references:
//!
//! - [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md)
//! - [`RawPoolAccessExtBase::raw_with_client`]

use crate::context::DjogiContext;
use crate::pg::connection::PgConnection;
use crate::pg::decode::{FromPgRow, try_get_scalar};
use crate::pg::pool::{ClientFuture, DjogiPool};
use crate::query::stream::{DEFAULT_FETCH_SIZE, RawCursorStream, build_raw_stream};
use crate::{DbError, DjogiError};
use postgres_types::{FromSql, ToSql};
use tokio_postgres::Row;

fn reject_transaction_session_statement(
    ctx: &mut DjogiContext,
    sql: &str,
) -> Result<(), DjogiError> {
    if let Some(err) = ctx.transaction_poison_error() {
        return Err(err);
    }
    if ctx.conn().is_some()
        && let Some(statement) = classify_transaction_session_statement(sql)
    {
        return Err(DjogiError::SessionStatementDisallowedInTransaction { statement });
    }
    Ok(())
}

fn reject_transaction_session_statement_batch(
    ctx: &mut DjogiContext,
    sql: &str,
) -> Result<(), DjogiError> {
    if let Some(err) = ctx.transaction_poison_error() {
        return Err(err);
    }
    if ctx.conn().is_some()
        && let Some(statement) = classify_raw_ddl_transaction_session_statement(sql)
    {
        return Err(DjogiError::SessionStatementDisallowedInTransaction { statement });
    }
    Ok(())
}

fn classify_transaction_session_statement(sql: &str) -> Option<&'static str> {
    let (keyword, next_idx) = parse_keyword(sql, 0)?;

    if keyword.eq_ignore_ascii_case("SET") {
        let second = parse_keyword(sql, next_idx).map(|(word, _)| word);
        return match second {
            Some(word)
                if word.eq_ignore_ascii_case("LOCAL")
                    || word.eq_ignore_ascii_case("CONSTRAINTS")
                    || word.eq_ignore_ascii_case("TRANSACTION") =>
            {
                None
            }
            _ => Some("SET"),
        };
    }

    [
        "RESET",
        "DISCARD",
        "LISTEN",
        "UNLISTEN",
        "PREPARE",
        "DEALLOCATE",
    ]
    .into_iter()
    .find(|statement| keyword.eq_ignore_ascii_case(statement))
}

fn classify_raw_ddl_transaction_session_statement(sql: &str) -> Option<&'static str> {
    let bytes = sql.as_bytes();
    let mut statement_start = 0usize;
    let mut idx = 0usize;
    let mut block_comment_depth = 0usize;
    let mut in_line_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_quote: Option<String> = None;

    while idx < bytes.len() {
        if let Some(delimiter) = dollar_quote.as_deref() {
            if sql[idx..].starts_with(delimiter) {
                idx += delimiter.len();
                dollar_quote = None;
            } else {
                idx += 1;
            }
            continue;
        }

        if in_line_comment {
            if bytes[idx] == b'\n' {
                in_line_comment = false;
            }
            idx += 1;
            continue;
        }

        if block_comment_depth > 0 {
            if bytes.get(idx) == Some(&b'/') && bytes.get(idx + 1) == Some(&b'*') {
                block_comment_depth += 1;
                idx += 2;
            } else if bytes.get(idx) == Some(&b'*') && bytes.get(idx + 1) == Some(&b'/') {
                block_comment_depth -= 1;
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }

        if in_single_quote {
            if bytes[idx] == b'\'' {
                if bytes.get(idx + 1) == Some(&b'\'') {
                    idx += 2;
                } else {
                    in_single_quote = false;
                    idx += 1;
                }
            } else {
                idx += 1;
            }
            continue;
        }

        if in_double_quote {
            if bytes[idx] == b'"' {
                if bytes.get(idx + 1) == Some(&b'"') {
                    idx += 2;
                } else {
                    in_double_quote = false;
                    idx += 1;
                }
            } else {
                idx += 1;
            }
            continue;
        }

        match bytes[idx] {
            b';' => {
                if let Some(statement) =
                    classify_transaction_session_statement(&sql[statement_start..idx])
                {
                    return Some(statement);
                }
                statement_start = idx + 1;
                idx += 1;
            }
            b'\'' => {
                in_single_quote = true;
                idx += 1;
            }
            b'"' => {
                in_double_quote = true;
                idx += 1;
            }
            b'-' if bytes.get(idx + 1) == Some(&b'-') => {
                in_line_comment = true;
                idx += 2;
            }
            b'/' if bytes.get(idx + 1) == Some(&b'*') => {
                block_comment_depth = 1;
                idx += 2;
            }
            b'$' => {
                if let Some(end_idx) = parse_dollar_quote_delimiter_end(sql, idx) {
                    dollar_quote = Some(sql[idx..end_idx].to_owned());
                    idx = end_idx;
                } else {
                    idx += 1;
                }
            }
            _ => {
                idx += 1;
            }
        }
    }

    classify_transaction_session_statement(&sql[statement_start..])
}

fn parse_keyword(sql: &str, start_idx: usize) -> Option<(&str, usize)> {
    let bytes = sql.as_bytes();
    let mut idx = skip_sql_trivia(sql, start_idx);
    if idx >= bytes.len() || !bytes[idx].is_ascii_alphabetic() {
        return None;
    }
    let start = idx;
    idx += 1;
    while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_') {
        idx += 1;
    }
    Some((&sql[start..idx], idx))
}

fn skip_sql_trivia(sql: &str, start_idx: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut idx = start_idx;

    loop {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }

        if bytes.get(idx) == Some(&b'-') && bytes.get(idx + 1) == Some(&b'-') {
            idx += 2;
            while idx < bytes.len() && bytes[idx] != b'\n' {
                idx += 1;
            }
            continue;
        }

        if bytes.get(idx) == Some(&b'/') && bytes.get(idx + 1) == Some(&b'*') {
            idx += 2;
            let mut depth = 1usize;
            while idx < bytes.len() && depth > 0 {
                if bytes.get(idx) == Some(&b'/') && bytes.get(idx + 1) == Some(&b'*') {
                    depth += 1;
                    idx += 2;
                } else if bytes.get(idx) == Some(&b'*') && bytes.get(idx + 1) == Some(&b'/') {
                    depth -= 1;
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            continue;
        }

        return idx;
    }
}

fn parse_dollar_quote_delimiter_end(sql: &str, start_idx: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes.get(start_idx) != Some(&b'$') {
        return None;
    }

    let mut idx = start_idx + 1;
    while idx < bytes.len() {
        match bytes[idx] {
            b'$' => return Some(idx + 1),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' => idx += 1,
            _ => return None,
        }
    }

    None
}

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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]` (under
    /// `tests/` the attribute requires a paired `// JUSTIFICATION (djogi#<n>):`
    /// comment, validated by `cargo xtask check-justifications`). See the
    /// [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    ///
    /// Prefer [`raw_query`](RawAccessExtBase::raw_query) when the row shape
    /// fits a `FromPgRow` impl — the per-row decode is positional and
    /// debug-asserted. Reach for `raw_rows` only when the caller really does
    /// need to inspect column metadata or call [`tokio_postgres::Row::try_get`]
    /// on heterogenous columns by name (e.g. dynamic introspection helpers).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    ///
    /// `raw_ddl` is `batch_execute(sql)` under a friendlier name — it
    /// carries the same blast radius as [`raw_execute`](RawAccessExtBase::raw_execute)
    /// and intentionally does not project through the migration substrate.
    /// When the context is already transaction-backed, Djogi preflights each
    /// top-level statement in the batch and rejects session-scoped statement
    /// heads (`SET`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`, `PREPARE`,
    /// `DEALLOCATE`) with
    /// [`DjogiError::SessionStatementDisallowedInTransaction`] before SQL
    /// reaches Postgres. The scanner respects comments, string literals, and
    /// dollar-quoted bodies; it is not a naive `split(';')`.
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
        reject_transaction_session_statement(self, sql)?;
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
        reject_transaction_session_statement(self, sql)?;
        // No post-query decode — the existing `query_all` guard already
        // covers the only Err/cancel exit shape.
        self.__query_all_for_macros(sql, params).await
    }

    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError> {
        reject_transaction_session_statement(self, sql)?;
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
        reject_transaction_session_statement(self, sql)?;
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
        reject_transaction_session_statement(self, sql)?;
        self.__execute_for_macros(sql, params).await
    }

    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError> {
        reject_transaction_session_statement_batch(self, sql)?;
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    ///
    /// Returns `None` when the context is pool-backed — there is no
    /// long-lived connection to borrow — and also when a transaction-backed
    /// context has been poisoned by a nested `atomic()` cancellation. Use for
    /// connection-state inspection (savepoint depth, in-progress transaction
    /// state) when an adopter-side helper needs to branch on the inner state.
    /// Prefer
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
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    ///
    /// `raw_with_client` is the framework's only path to the underlying
    /// `tokio_postgres::Client` and the canonical public route for
    /// driver-level operations that the typed surface cannot express:
    /// `client.copy_in(...)`, `client.copy_out(...)`, server-side cursor
    /// protocol work, `client.simple_query(...)`, `CREATE EXTENSION`, and
    /// third-party helpers that require a `&tokio_postgres::Client`. The
    /// closure receives `&mut Client` for the duration of the borrow; keep
    /// the full COPY/cursor exchange inside that closure so the pool guard
    /// can return the connection on `Ok` or detach it on `Err`, panic, or
    /// cancellation. Typed-surface COPY and streaming wrappers do not exist
    /// today; this explicit bypass is the supported public route for those
    /// driver-level operations.
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
        if self.is_transaction_poisoned() {
            None
        } else {
            self.conn()
        }
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

#[cfg(test)]
mod tests {
    use super::{
        classify_raw_ddl_transaction_session_statement, classify_transaction_session_statement,
    };

    #[test]
    fn classify_transaction_session_statement_rejects_plain_set_after_leading_comments() {
        let sql = "  /* prelude ; */ -- line comment\n  sEt search_path = public";
        assert_eq!(classify_transaction_session_statement(sql), Some("SET"));
    }

    #[test]
    fn classify_transaction_session_statement_allows_transaction_local_set_forms() {
        assert_eq!(
            classify_transaction_session_statement("SET LOCAL statement_timeout = '5s'"),
            None
        );
        assert_eq!(
            classify_transaction_session_statement("SET CONSTRAINTS ALL IMMEDIATE"),
            None
        );
        assert_eq!(
            classify_transaction_session_statement(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
            ),
            None
        );
    }

    #[test]
    fn classify_transaction_session_statement_rejects_other_session_statement_heads() {
        for (sql, expected) in [
            ("RESET ALL", "RESET"),
            ("discard all", "DISCARD"),
            ("LISTEN djogi_updates", "LISTEN"),
            ("unlisten *", "UNLISTEN"),
            ("PREPARE x AS SELECT 1", "PREPARE"),
            ("deallocate all", "DEALLOCATE"),
        ] {
            assert_eq!(
                classify_transaction_session_statement(sql),
                Some(expected),
                "expected {expected} to be rejected for {sql:?}"
            );
        }
    }

    #[test]
    fn classify_raw_ddl_transaction_session_statement_ignores_semicolons_inside_bodies() {
        let sql = r#"
            DO $body$
            BEGIN
                PERFORM '; still inside the body';
                PERFORM $$nested ; dollar quote$$;
            END
            $body$;
            /* scanner must only inspect the real next statement */
            LISTEN djogi_updates;
        "#;

        assert_eq!(
            classify_raw_ddl_transaction_session_statement(sql),
            Some("LISTEN")
        );
    }

    #[test]
    fn classify_raw_ddl_transaction_session_statement_allows_trivia_only_and_safe_batches() {
        assert_eq!(
            classify_raw_ddl_transaction_session_statement(
                " /* nothing here */ \n -- still nothing\n"
            ),
            None
        );

        let sql = r#"
            DO $body$
            BEGIN
                PERFORM '; safe body';
            END
            $body$;
            CREATE TEMP TABLE djogi_282_classifier_ok (value integer);
        "#;
        assert_eq!(classify_raw_ddl_transaction_session_statement(sql), None);
    }
}
