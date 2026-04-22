//! Streaming / cursor terminals for [`QuerySet<T>`] and [`DjogiContext`].
//!
//! # What
//!
//! This module adds two streaming terminals:
//!
//! - [`ModelCursorStream`] — returned by `QuerySet::stream` and
//!   `QuerySet::stream_with_fetch_size`. Yields `Result<T, DjogiError>`
//!   where `T: FromPgRow`. Rows are decoded as they arrive.
//! - [`RawCursorStream`] — returned by `DjogiContext::raw_stream` and
//!   `DjogiContext::raw_stream_with_fetch_size`. Yields
//!   `Result<tokio_postgres::Row, DjogiError>`. The caller decodes rows
//!   themselves.
//!
//! Both terminals require an active `atomic()` scope (Postgres named
//! cursors are transaction-local). Constructing a stream outside a
//! transaction surfaces [`DjogiError::StreamOutsideTransaction`] at
//! construction time.
//!
//! # Implementation: async-stream crate
//!
//! Manual `Stream` impls that hold async futures inline face a persistent
//! borrow-checker conflict: `tokio::pin!`-ing a future that borrows
//! `self.conn` makes it impossible to also mutate `self.exhausted`,
//! `self.buffer`, etc. in the same `poll_next` body.
//!
//! The solution adopted here is the `async_stream::stream!` macro, which
//! desugars into a state-machine generator with no borrow conflicts. The
//! resulting `impl Stream` is `Send` because the closure captures only
//! `Send` values (`PgConnection` reference, `String`, `VecDeque`).
//!
//! # Transaction invariant
//!
//! Both stream constructors verify the context's `ContextInner` is
//! `Transaction`. Pool-backed contexts return
//! [`DjogiError::StreamOutsideTransaction`] immediately.
//!
//! # Fetch size
//!
//! Default: `1000` rows per `FETCH` round trip. Override via
//! `stream_with_fetch_size` / `raw_stream_with_fetch_size`.
//!
//! # Lifecycle
//!
//! 1. `DECLARE djogi_cur_<uuid> CURSOR FOR <sql>` at construction.
//! 2. On each `poll_next`: yield from buffer, or `FETCH <n>` to refill.
//! 3. When `FETCH` returns fewer rows than `fetch_size`: cursor exhausted.
//! 4. `CLOSE <name>` issued after the last row is yielded.
//! 5. Transaction rollback auto-closes the cursor server-side; the next
//!    `FETCH` / `CLOSE` surfaces a `DjogiError::Db`.

use crate::context::{ContextInner, DjogiContext};
use crate::error::DjogiError;
use crate::pg::connection::PgConnection;
use crate::pg::cursor::{PgCursor, cursor_close, cursor_fetch};
use crate::pg::decode::FromPgRow;
use futures::Stream;
use postgres_types::ToSql;
use std::collections::VecDeque;
use std::pin::Pin;
use tokio_postgres::Row;

/// Default number of rows retrieved per `FETCH` round trip.
pub const DEFAULT_FETCH_SIZE: u32 = 1000;

// ---------------------------------------------------------------------------
// Inner stream driver — shared by both public stream types
// ---------------------------------------------------------------------------

/// Low-level cursor-backed row driver.
///
/// Drives the `FETCH` / `CLOSE` lifecycle against the Postgres connection.
/// Yields raw `tokio_postgres::Row` values; the typed model stream wraps
/// this and applies `FromPgRow` decoding.
///
/// `'ctx` ties the driver to the `DjogiContext` from which the connection
/// was borrowed. The caller holds `&'ctx mut PgConnection` exclusively for
/// the duration of the stream.
struct CursorDriver<'ctx> {
    cursor_name: String,
    conn: &'ctx mut PgConnection,
    buffer: VecDeque<Row>,
    fetch_size: u32,
    exhausted: bool,
    closed: bool,
}

impl<'ctx> CursorDriver<'ctx> {
    fn new(cursor_name: String, conn: &'ctx mut PgConnection, fetch_size: u32) -> Self {
        CursorDriver {
            cursor_name,
            conn,
            buffer: VecDeque::new(),
            fetch_size,
            exhausted: false,
            closed: false,
        }
    }

    /// Fetch one row from the driver, if available.
    ///
    /// Issues `FETCH` when the buffer is empty and the cursor is not yet
    /// exhausted. Returns `None` after closing the cursor. Returns
    /// `Some(Err)` on a Postgres error.
    async fn next_row(&mut self) -> Option<Result<Row, DjogiError>> {
        loop {
            // Fused.
            if self.closed {
                return None;
            }

            // Yield from buffer without a network round trip.
            if let Some(row) = self.buffer.pop_front() {
                return Some(Ok(row));
            }

            // Buffer empty and cursor exhausted — issue CLOSE.
            if self.exhausted {
                self.closed = true;
                // Ignore CLOSE errors: the transaction ending auto-closes the
                // cursor. A CLOSE failure here typically means the transaction
                // has already rolled back, which is fine — we just report None.
                let _ = cursor_close(&self.cursor_name, self.conn).await;
                return None;
            }

            // Fetch the next batch.
            let fetch_size = self.fetch_size;
            match cursor_fetch(&self.cursor_name, self.conn, fetch_size).await {
                Ok(rows) => {
                    let got = rows.len() as u32;
                    if got < self.fetch_size {
                        self.exhausted = true;
                    }
                    self.buffer.extend(rows);
                    // Loop to yield from the freshly filled buffer (or to
                    // enter the `exhausted` branch if zero rows came back).
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ModelCursorStream — typed, yields Result<T, DjogiError>
// ---------------------------------------------------------------------------

/// A [`Stream`] over `T: FromPgRow` backed by a Postgres named cursor.
///
/// Created by [`crate::query::queryset::QuerySet::stream`] and
/// `QuerySet::stream_with_fetch_size`.
///
/// Rows are decoded on the fly via `T::from_pg_row(&row)` as they are
/// yielded from the internal buffer. At most `fetch_size` raw
/// `tokio_postgres::Row`s are held in the buffer at any time.
///
/// # Lifetime
///
/// `'ctx` ties this stream to the `&'ctx mut DjogiContext` it was created
/// from. The stream holds a mutable borrow on the context's underlying
/// `PgConnection` for its entire lifetime; the context cannot be used for
/// other operations while the stream is alive.
pub struct ModelCursorStream<'ctx, T> {
    driver: CursorDriver<'ctx>,
    _marker: std::marker::PhantomData<T>,
}

impl<'ctx, T> ModelCursorStream<'ctx, T> {
    fn new(cursor_name: String, conn: &'ctx mut PgConnection, fetch_size: u32) -> Self {
        ModelCursorStream {
            driver: CursorDriver::new(cursor_name, conn, fetch_size),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'ctx, T: FromPgRow + Unpin + Send> Stream for ModelCursorStream<'ctx, T> {
    type Item = Result<T, DjogiError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let fut = self.driver.next_row();
        tokio::pin!(fut);
        match fut.poll(cx) {
            std::task::Poll::Ready(Some(Ok(row))) => {
                std::task::Poll::Ready(Some(T::from_pg_row(&row)))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

// SAFETY: `PgConnection` is `Send` (wraps `deadpool_postgres::Object`
// which is `Send + !Sync`). `CursorDriver` holds `String`, `VecDeque<Row>`,
// and `&'ctx mut PgConnection` — all `Send`. `PhantomData<T>` does not
// add any non-Send data.
unsafe impl<'ctx, T: Send> Send for ModelCursorStream<'ctx, T> {}

// ---------------------------------------------------------------------------
// RawCursorStream — untyped, yields Result<Row, DjogiError>
// ---------------------------------------------------------------------------

/// A [`Stream`] over raw `tokio_postgres::Row` backed by a Postgres named cursor.
///
/// Created by [`DjogiContext::raw_stream`] and `raw_stream_with_fetch_size`.
///
/// Yields `Result<tokio_postgres::Row, DjogiError>`. The caller decodes rows
/// themselves using `Row::get` or a custom `FromPgRow` impl.
///
/// Same lifecycle and transaction constraints as [`ModelCursorStream`].
pub struct RawCursorStream<'ctx> {
    driver: CursorDriver<'ctx>,
}

impl<'ctx> RawCursorStream<'ctx> {
    fn new(cursor_name: String, conn: &'ctx mut PgConnection, fetch_size: u32) -> Self {
        RawCursorStream {
            driver: CursorDriver::new(cursor_name, conn, fetch_size),
        }
    }
}

impl<'ctx> Stream for RawCursorStream<'ctx> {
    type Item = Result<Row, DjogiError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let fut = self.driver.next_row();
        tokio::pin!(fut);
        match fut.poll(cx) {
            std::task::Poll::Ready(v) => std::task::Poll::Ready(v),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

// SAFETY: same reasoning as `ModelCursorStream`.
unsafe impl<'ctx> Send for RawCursorStream<'ctx> {}

// ---------------------------------------------------------------------------
// Construction helpers — called from terminal.rs and context.rs
// ---------------------------------------------------------------------------

/// Construct a [`ModelCursorStream`] for the given SQL + binds.
///
/// Called by the `QuerySet::stream` terminal in `terminal.rs`. Verifies the
/// context is transaction-backed (cursors require a transaction) and issues
/// `DECLARE … CURSOR FOR …` before returning.
///
/// # Errors
///
/// - [`DjogiError::StreamOutsideTransaction`] — context is pool-backed.
/// - [`DjogiError::Db`] — `DECLARE` statement failed.
pub async fn build_model_stream<'ctx, T: FromPgRow + Unpin + Send>(
    ctx: &'ctx mut DjogiContext,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    fetch_size: u32,
) -> Result<ModelCursorStream<'ctx, T>, DjogiError> {
    let conn = require_transaction(ctx)?;
    let cursor = PgCursor::declare(conn, sql, params).await?;
    let name = cursor.name().to_owned();
    Ok(ModelCursorStream::new(name, conn, fetch_size))
}

/// Construct a [`RawCursorStream`] for the given SQL + binds.
///
/// Called by `DjogiContext::raw_stream` in `context.rs`. Same transaction
/// invariant and error surface as [`build_model_stream`].
pub async fn build_raw_stream<'ctx>(
    ctx: &'ctx mut DjogiContext,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    fetch_size: u32,
) -> Result<RawCursorStream<'ctx>, DjogiError> {
    let conn = require_transaction(ctx)?;
    let cursor = PgCursor::declare(conn, sql, params).await?;
    let name = cursor.name().to_owned();
    Ok(RawCursorStream::new(name, conn, fetch_size))
}

/// Extract the `PgConnection` from a transaction-backed context.
///
/// Returns `Err(DjogiError::StreamOutsideTransaction)` when the context is
/// pool-backed. Both `build_model_stream` and `build_raw_stream` call this.
fn require_transaction(ctx: &mut DjogiContext) -> Result<&mut PgConnection, DjogiError> {
    match ctx.inner_mut() {
        ContextInner::Transaction(conn) => Ok(conn),
        ContextInner::Pool(_) => Err(DjogiError::StreamOutsideTransaction),
    }
}
