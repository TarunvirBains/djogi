//! Postgres named-cursor helpers — DECLARE, FETCH, CLOSE.
//!
//! # What
//!
//! A [`PgCursor`] wraps the lifecycle of a Postgres named cursor opened inside
//! an active transaction. Postgres named cursors are transaction-local: they
//! live for the duration of the enclosing transaction, then vanish when the
//! transaction commits or rolls back (Postgres auto-closes them). This module
//! provides the low-level primitives that the higher-level `QuerySet::stream`
//! terminal builds on.
//!
//! # Why a separate module
//!
//! The cursor lifecycle (`DECLARE … CURSOR FOR …`, `FETCH n FROM …`,
//! `CLOSE …`) involves three distinct SQL statements and a state machine with
//! four meaningful states (`Declared`, `Fetching`, `Exhausted`, `Closed`).
//! Keeping this in its own file — rather than embedding the state machine
//! inside `stream.rs` — makes each file auditable: `cursor.rs` owns the
//! Postgres wire protocol for cursors, `stream.rs` owns the `Stream` trait
//! machinery and the glue between cursors and `FromPgRow` decoding.
//!
//! # Cursor-name format
//!
//! Every cursor gets a unique name of the form `djogi_cur_<32 hex chars>`.
//! The name is generated from a fresh `Uuid::new_v4()` using the
//! `simple()` formatter (no hyphens), prefixed by `djogi_cur_`. The result
//! is always an ASCII identifier containing only lowercase letters, digits,
//! and underscores — safe as an unquoted Postgres identifier. Validation
//! follows the rule: every byte after the prefix must be ASCII alphanumeric
//! (letter or digit); underscores are accepted only in the prefix. No regex
//! is used — just byte-level checks via `u8::is_ascii_alphanumeric`.
//!
//! # Transaction invariant
//!
//! Callers MUST only create a `PgCursor` from a `PgConnection` that
//! has an open transaction. Declaring a cursor outside a transaction is
//! a Postgres error (`ERROR: cursor can only scan forward`). The
//! `QuerySet::stream` terminal enforces this by checking
//! `ContextInner::Transaction` before calling `PgCursor::declare`.
//!
//! # Fetch size
//!
//! `FETCH <n> FROM <cursor>` retrieves up to `n` rows per round trip.
//! The default is `1000`. Callers may choose a smaller value (e.g. `100`)
//! for testing or memory-sensitive workloads; a larger value (e.g. `10_000`)
//! for high-throughput exports where network round-trip cost dominates.

use crate::pg::connection::PgConnection;
use crate::{DbError, DjogiError};
use tokio_postgres::Row;
use uuid::Uuid;

/// A Postgres named cursor opened inside a transaction.
///
/// Created by [`PgCursor::declare`]. Rows are retrieved by [`PgCursor::fetch`].
/// The cursor must be closed by calling [`PgCursor::close`] when done; failing
/// to close is not a resource leak in the strictest sense (the transaction
/// ending closes it), but an explicit `CLOSE` is best practice and enables the
/// `stream_drop_closes_cursor` integration test to query `pg_cursors`.
///
/// `PgCursor` does NOT implement `Drop` to issue `CLOSE` because `close`
/// is async; the caller (the stream) must drive the close explicitly in its
/// drop logic via a separate mechanism. See `stream.rs` for the channel-based
/// close notification pattern.
pub struct PgCursor {
    /// The unique cursor name — ASCII alphanumeric plus underscore prefix.
    name: String,
}

/// Generate a unique cursor name.
///
/// The name is `djogi_cur_` followed by the `simple` (no-hyphens) form of
/// a new random UUID. All 32 hex characters after the prefix are ASCII
/// lowercase alphanumeric bytes. Total length: 10 + 32 = 42 bytes, well
/// within Postgres's 63-byte identifier limit.
///
/// Validation proof (no regex): The UUID simple form is always exactly
/// 32 lowercase hex digits (ASCII bytes in `[0-9a-f]`). Each byte satisfies
/// `b.is_ascii_alphanumeric()`. The prefix `djogi_cur_` contains only ASCII
/// letters and underscores. Therefore the whole name is a valid unquoted
/// Postgres identifier.
pub fn generate_cursor_name() -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    // Verify every byte of the generated suffix is ASCII alphanumeric.
    // `Uuid::simple().to_string()` always produces 32 lowercase hex chars,
    // so this assertion fires only on a broken UUID implementation — it is
    // a debug guard, not a performance-sensitive path.
    debug_assert!(
        suffix.bytes().all(|b| b.is_ascii_alphanumeric()),
        "UUID simple() format unexpectedly produced a non-alphanumeric byte"
    );
    format!("djogi_cur_{suffix}")
}

impl PgCursor {
    /// Declare a named Postgres cursor for `sql` with `params` binds.
    ///
    /// Issues `DECLARE <name> CURSOR FOR <sql>` against the provided
    /// `PgConnection`. The connection MUST be inside an open transaction;
    /// enforced by the caller (`QuerySet::stream` checks
    /// `ContextInner::Transaction` before calling here).
    ///
    /// # SQL construction
    ///
    /// The cursor name is composed from a framework-generated UUID suffix —
    /// it is never user-supplied. The `sql` argument is the output of
    /// `build_select`, which also never interpolates user data (all filter
    /// values flow through `$n` bind slots). The `DECLARE … CURSOR FOR …`
    /// statement itself carries the full bind list from the original SELECT.
    ///
    /// # Returns
    ///
    /// The constructed `PgCursor` handle, or a `DjogiError::Db` on the
    /// `DECLARE` round trip failure.
    pub async fn declare(
        conn: &mut PgConnection,
        sql: &str,
        params: &[&(dyn postgres_types::ToSql + Sync)],
    ) -> Result<Self, DjogiError> {
        let name = generate_cursor_name();
        let declare_sql = format!("DECLARE {name} CURSOR FOR {sql}");
        // `DECLARE … CURSOR FOR …` with parameter binds goes through the
        // extended query protocol (prepare + bind). tokio-postgres's
        // `execute` path does this correctly.
        conn.execute(&declare_sql, params).await?;
        Ok(PgCursor { name })
    }

    /// Fetch up to `fetch_size` rows from this cursor.
    ///
    /// Issues `FETCH <fetch_size> FROM <name>`. Returns an empty `Vec`
    /// when the cursor is exhausted (no more rows). Returns a non-empty
    /// `Vec<Row>` otherwise.
    ///
    /// The `fetch_size` value is a `u32` that is formatted into the SQL
    /// string as a literal integer. It is never user-supplied (callers
    /// use `stream_with_fetch_size` which validates the value is >= 1).
    pub async fn fetch(
        &self,
        conn: &mut PgConnection,
        fetch_size: u32,
    ) -> Result<Vec<Row>, DjogiError> {
        cursor_fetch(&self.name, conn, fetch_size).await
    }

    /// Close this cursor.
    ///
    /// Issues `CLOSE <name>`. After this call the cursor name is no longer
    /// visible in `pg_cursors`. Calling `close` twice or calling it on a
    /// cursor whose transaction has already committed / rolled back produces
    /// a Postgres error that is mapped to `DjogiError::Db`.
    pub async fn close(&self, conn: &mut PgConnection) -> Result<(), DjogiError> {
        cursor_close(&self.name, conn).await
    }

    /// The cursor's name — `djogi_cur_<32 hex chars>`.
    ///
    /// Exposed so tests can query `pg_cursors` to verify the cursor was
    /// opened / closed.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Issue `FETCH <fetch_size> FROM <cursor_name>` on `conn`.
///
/// Free function so stream types can call it with split borrows:
/// `cursor_fetch(self.cursor.name(), self.conn, self.fetch_size)` avoids
/// the borrow-checker conflict that arises when `fetch` is a method on
/// `PgCursor` taking `&mut PgConnection` while both `cursor` and `conn` are
/// fields of the same struct (the borrow checker cannot see they don't alias
/// through a shared `&mut self`).
pub async fn cursor_fetch(
    cursor_name: &str,
    conn: &mut PgConnection,
    fetch_size: u32,
) -> Result<Vec<Row>, DjogiError> {
    let fetch_sql = format!("FETCH {} FROM {}", fetch_size, cursor_name);
    conn.query(&fetch_sql, &[]).await
}

/// Issue `CLOSE <cursor_name>` on `conn`.
///
/// Free function for the same split-borrow reason as [`cursor_fetch`].
pub async fn cursor_close(cursor_name: &str, conn: &mut PgConnection) -> Result<(), DjogiError> {
    let close_sql = format!("CLOSE {}", cursor_name);
    conn.batch_execute(&close_sql).await.map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "CLOSE cursor {cursor_name} failed: {e}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_name_has_correct_prefix() {
        let name = generate_cursor_name();
        assert!(
            name.starts_with("djogi_cur_"),
            "cursor name must start with 'djogi_cur_', got: {name}"
        );
    }

    #[test]
    fn cursor_name_suffix_is_alphanumeric() {
        let name = generate_cursor_name();
        // Skip the prefix "djogi_cur_" (10 bytes) and verify the rest.
        let suffix = &name["djogi_cur_".len()..];
        assert_eq!(suffix.len(), 32, "UUID simple form must be 32 chars");
        for b in suffix.bytes() {
            assert!(
                b.is_ascii_alphanumeric(),
                "cursor name suffix byte {b} is not ASCII alphanumeric"
            );
        }
    }

    #[test]
    fn cursor_name_total_length_within_postgres_limit() {
        let name = generate_cursor_name();
        assert!(
            name.len() <= 63,
            "cursor name must be at most 63 bytes (Postgres identifier limit), got {} bytes",
            name.len()
        );
    }

    #[test]
    fn two_cursor_names_are_distinct() {
        let a = generate_cursor_name();
        let b = generate_cursor_name();
        assert_ne!(
            a, b,
            "two cursor names from separate calls must be distinct"
        );
    }
}
