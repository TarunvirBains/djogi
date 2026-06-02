//! Deliberate raw SQL escape hatches — djogi's `unsafe`-equivalent.
//! Raw SQL in djogi is not banned, but it is intentionally loud. This module
//! is public only so adopters, pin tests, and sibling workspace crates can opt
//! in consciously; it is hidden from rustdoc and its traits are sealed. The
//! supported unlock is `#[djogi::deliberately_bypass_convention_with_raw_sql]`
//! plus an adjacent `// JUSTIFICATION ...` comment. Under `tests/`, `cargo
//! xtask check-justifications` enforces that convention.
//! Adopter code reaches these methods through the bypass attribute, not by
//! importing `djogi::__bypass::*` directly:
//! ```ignore
//! use djogi::prelude::*;
//!
//! #[djogi::deliberately_bypass_convention_with_raw_sql]
//! // JUSTIFICATION (djogi#234): typed surface lacks recursive CTE support.
//! async fn count_users_ci(ctx: &mut DjogiContext, name: &str) -> djogi::Result<i64> {
//! ctx.raw_scalar(
//! "SELECT COUNT(*) FROM users WHERE LOWER(name) = LOWER($1)",
//! &[&name],
//! ).await
//! }
//! ```
//! # Pool-backed lifecycle
//! Pool-backed raw methods (`raw_query`, `raw_rows`, `raw_fetch_one`,
//! `raw_scalar`, `raw_execute`, `raw_ddl`) route through the same
//! dirty-by-default pool guards as
//! [`crate::pg::pool::DjogiPool::with_client`]:
//! - `Ok` returns the connection to the pool normally.
//! - `Err`, panic, future cancellation, and post-query decode failure detach
//! the connection instead of recycling it.
//! This is required because Djogi uses
//! `deadpool_postgres::RecyclingMethod::Fast`, which only checks
//! `is_closed` on return; it does **not** issue `ROLLBACK`, `RESET ALL`, or
//! `DISCARD ALL`. The extra detach cost on dirty exits is what prevents a
//! poisoned session from leaking to the next checkout.
//! Even with that guard, a session-state mutation that returns `Ok` on a
//! pool-backed context still leaves the session non-default on the clean path.
//! If the caller deliberately runs session-scoped raw SQL outside a
//! transaction, they still own the cleanup contract.
//! # Transaction-backed contract
//! When the context is already inside [`crate::transaction::atomic`], djogi no
//! longer treats session-scoped raw SQL as "caller cleans it up later". The
//! bypass layer preflights transaction-backed `raw_query`, `raw_rows`,
//! `raw_fetch_one`, `raw_scalar`, `raw_execute`, and `raw_ddl` and rejects
//! these statement heads with
//! [`crate::DjogiError::SessionStatementDisallowedInTransaction`] before SQL
//! reaches Postgres:
//! - plain `SET`
//! - `RESET`
//! - `DISCARD`
//! - `LISTEN`
//! - `UNLISTEN`
//! - `PREPARE`
//! - `DEALLOCATE`
//! Transaction-local forms remain allowed: `SET LOCAL ...`,
//! `SET CONSTRAINTS ...`, and `SET TRANSACTION ...`.
//! Transaction-control statements are also rejected with
//! [`crate::DjogiError::RawTransactionControlDisallowedInTransaction`] to
//! prevent callers from bypassing framework bookkeeping (on_commit callback
//! drain, rollback cleanup, savepoint depth synchronization):
//! - `BEGIN` / `START TRANSACTION` (`BEGIN ATOMIC` is excluded — it is a
//! compound-statement delimiter, not transaction control)
//! - `COMMIT`
//! - `ROLLBACK` (including `ROLLBACK TO savepoint`)
//! - `END` / `ABORT`
//! - `SAVEPOINT`
//! - `RELEASE SAVEPOINT` / bare `RELEASE`
//! The refusal is intentionally conservative:
//! - it applies only to transaction-backed raw entrypoints
//! - empty/trivia-only SQL passes through unchanged
//! - `raw_ddl` scans real top-level statements, respecting line comments,
//! block comments, quoted strings, dollar-quoted bodies, and
//! `BEGIN ATOMIC ... END` compound-statement boundaries
//! - `raw_stream` / `raw_stream_with_fetch_size` keep their existing
//! transaction-required contract and do not run this classifier
//! # Cancellation and poison
//! Top-level `atomic` cancellation no longer recycles an open transaction:
//! the dirty-drop guard detaches the connection on cancellation before it can
//! leak back to the pool.
//! Nested `atomic` cancellation remains fail-closed. If a nested future is
//! dropped before savepoint cleanup runs, the outer transaction is poisoned,
//! framework-owned work rejects further use, and `commit` rolls back instead
//! of committing. `raw_conn` therefore returns `None` both for pool-backed
//! contexts and for poisoned transaction-backed contexts.
//! Cursors, `COPY`, binary-protocol helpers, and other multi-round-trip driver
//! work should go through [`RawPoolAccessExt::raw_with_client`], which bounds
//! the protocol exchange to one checkout and applies the same dirty-detach
//! policy on dirty exit.
//! Cross-references:
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

fn reject_transaction_backed_sql(ctx: &mut DjogiContext, sql: &str) -> Result<(), DjogiError> {
    if let Some(err) = ctx.transaction_poison_error() {
        return Err(err);
    }
    if ctx.conn().is_some()
        && let Some(refusal) = classify_transaction_backed_refusal(sql)
    {
        return Err(refusal.into_error());
    }
    Ok(())
}

fn reject_transaction_backed_sql_batch(
    ctx: &mut DjogiContext,
    sql: &str,
) -> Result<(), DjogiError> {
    if let Some(err) = ctx.transaction_poison_error() {
        return Err(err);
    }
    if ctx.conn().is_some()
        && let Some(refusal) = classify_raw_ddl_transaction_backed_refusal(sql)
    {
        return Err(refusal.into_error());
    }
    Ok(())
}

pub(crate) async fn guarded_batch_execute(
    ctx: &mut DjogiContext,
    sql: &str,
) -> Result<(), DjogiError> {
    reject_transaction_backed_sql_batch(ctx, sql)?;
    ctx.batch_execute(sql).await
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

#[allow(dead_code)] // Retained by existing regression tests; superseded in production by classify_raw_ddl_transaction_backed_refusal.
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
            if bytes[idx..].starts_with(delimiter.as_bytes()) {
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

// ---------------------------------------------------------------------------
// Transaction-control statement classifier (#306).
// ---------------------------------------------------------------------------

fn classify_transaction_control_statement(sql: &str) -> Option<&'static str> {
    let (first, after_first) = parse_keyword(sql, 0)?;

    if first.eq_ignore_ascii_case("START") {
        return match parse_keyword(sql, after_first) {
            Some((second, _)) if second.eq_ignore_ascii_case("TRANSACTION") => {
                Some("START TRANSACTION")
            }
            _ => None,
        };
    }

    if first.eq_ignore_ascii_case("ROLLBACK") {
        let second = parse_keyword(sql, after_first);
        return match second {
            Some((w, _))
                if w.eq_ignore_ascii_case("WORK") || w.eq_ignore_ascii_case("TRANSACTION") =>
            {
                // ROLLBACK WORK / ROLLBACK TRANSACTION (plain rollback)
                // and ROLLBACK WORK TO sp / ROLLBACK TRANSACTION TO sp
                Some("ROLLBACK")
            }
            Some((w, _)) if w.eq_ignore_ascii_case("TO") => Some("ROLLBACK"),
            _ => Some("ROLLBACK"),
        };
    }

    if first.eq_ignore_ascii_case("RELEASE") {
        // Bare RELEASE or RELEASE SAVEPOINT — both are transaction control.
        return Some("RELEASE");
    }

    if first.eq_ignore_ascii_case("BEGIN") {
        return match parse_keyword(sql, after_first) {
            Some((second, _)) if second.eq_ignore_ascii_case("ATOMIC") => {
                // BEGIN ATOMIC is a SQL-standard compound-statement delimiter
                // (used in `LANGUAGE SQL` function bodies), not transaction
                // control. Postgres treats it differently from bare BEGIN,
                // which starts a transaction block.
                None
            }
            // Bare BEGIN, BEGIN WORK, BEGIN TRANSACTION are all transaction control.
            _ => Some("BEGIN"),
        };
    }

    ["COMMIT", "END", "ABORT", "SAVEPOINT"]
        .into_iter()
        .find(|s| first.eq_ignore_ascii_case(s))
}

// ---------------------------------------------------------------------------
// Unified refusal enum (#306).
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub(crate) enum TransactionBackedRawSqlRefusal {
    SessionStatement(&'static str),
    TransactionControl(&'static str),
}

impl TransactionBackedRawSqlRefusal {
    pub(crate) fn into_error(self) -> DjogiError {
        match self {
            Self::SessionStatement(s) => {
                DjogiError::SessionStatementDisallowedInTransaction { statement: s }
            }
            Self::TransactionControl(s) => {
                DjogiError::RawTransactionControlDisallowedInTransaction { statement: s }
            }
        }
    }
}

fn classify_transaction_backed_refusal(sql: &str) -> Option<TransactionBackedRawSqlRefusal> {
    if let Some(s) = classify_transaction_control_statement(sql) {
        return Some(TransactionBackedRawSqlRefusal::TransactionControl(s));
    }
    if let Some(s) = classify_transaction_session_statement(sql) {
        return Some(TransactionBackedRawSqlRefusal::SessionStatement(s));
    }
    None
}

fn classify_raw_ddl_transaction_backed_refusal(
    sql: &str,
) -> Option<TransactionBackedRawSqlRefusal> {
    let bytes = sql.as_bytes();
    let mut statement_start = 0usize;
    let mut idx = 0usize;
    let mut block_comment_depth = 0usize;
    let mut in_line_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_quote: Option<String> = None;
    // Depth of open `BEGIN ATOMIC ... END` compound-statement blocks. Inside
    // them, semicolons do not split statements and the closing END is not
    // transaction control.
    let mut begin_atomic_depth = 0usize;
    // Depth of open `CASE ... END` expressions inside the current atomic block,
    // so a CASE's END does not prematurely close the BEGIN ATOMIC block.
    let mut case_depth = 0usize;

    while idx < bytes.len() {
        if let Some(delimiter) = dollar_quote.as_deref() {
            if bytes[idx..].starts_with(delimiter.as_bytes()) {
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

        // Track BEGIN ATOMIC ... END compound-statement boundaries. Quote and
        // comment state is handled above, so this runs only on real, unquoted
        // SQL text.
        if begin_atomic_depth > 0 {
            // CASE ... END is the only bare-END construct that appears unquoted
            // inside a SQL-standard atomic body (e.g. SELECT CASE WHEN x > 0
            // THEN 1 ELSE 0 END). Count CASE opens so their END does not
            // prematurely close the atomic block. A qualified END such as
            // END IF or END LOOP belongs to PL/pgSQL and only appears inside
            // dollar-quoted function bodies, which existing dollar-quote
            // tracking already skips.
            if let Some(after_case) = match_keyword_at(bytes, idx, "CASE") {
                case_depth += 1;
                idx = after_case;
                continue;
            }

            // A bare END closes the innermost open CASE first; only when no
            // CASE is open does it close the BEGIN ATOMIC block itself.
            if let Some(after_end) = match_keyword_at(bytes, idx, "END") {
                if case_depth > 0 {
                    case_depth -= 1;
                } else {
                    begin_atomic_depth -= 1;
                    // Reset CASE depth as the block fully closes so no residual
                    // imbalance leaks across sequential atomic blocks in one
                    // batch.
                    if begin_atomic_depth == 0 {
                        case_depth = 0;
                    }
                }
                idx = after_end;
                continue;
            }

            // A nested BEGIN ATOMIC raises the depth.
            if let Some(after_begin) = match_keyword_at(bytes, idx, "BEGIN")
                && let Some(after_atomic) = skip_whitespace_and_match(bytes, after_begin, "ATOMIC")
            {
                begin_atomic_depth += 1;
                idx = after_atomic;
                continue;
            }

            // Semicolons inside the block belong to the compound statement
            // never split on them.
            if bytes[idx] == b';' {
                idx += 1;
                continue;
            }

            // Anything else falls through to the match block below so quote and
            // comment state is entered correctly: a string literal or comment
            // opened inside the block (a quoted 'END', a block comment, or a
            // line comment containing END) is skipped without touching depth.
        }

        // Outside any atomic block, an unquoted BEGIN ATOMIC opens one.
        if begin_atomic_depth == 0
            && let Some(after_begin) = match_keyword_at(bytes, idx, "BEGIN")
            && let Some(after_atomic) = skip_whitespace_and_match(bytes, after_begin, "ATOMIC")
        {
            begin_atomic_depth = 1;
            idx = after_atomic;
            continue;
        }

        match bytes[idx] {
            b';' => {
                if let Some(refusal) =
                    classify_transaction_backed_refusal(&sql[statement_start..idx])
                {
                    return Some(refusal);
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

    classify_transaction_backed_refusal(&sql[statement_start..])
}

/// Returns `true` when `b` can appear *inside* an unquoted PostgreSQL
/// identifier (i.e. as a non-leading identifier byte), and therefore must be
/// treated as part of an identifier for keyword-boundary purposes.
/// Per the PostgreSQL lexical rules, identifier continuation characters are
/// ASCII letters, ASCII digits, `_`, and `$`. PostgreSQL additionally permits
/// non-ASCII letters (any byte `> 127` in a UTF-8 source — either the lead
/// byte or a continuation byte of a multibyte identifier character). Treating
/// every non-ASCII byte as an identifier byte is the conservative choice for a
/// byte-level scanner: it guarantees a keyword is never matched at a position
/// that is actually inside a multibyte identifier character.
/// This is used for the word-boundary checks around keyword matches, so that
/// `BEGIN` in `x$begin`, `begin$x`, or a non-ASCII-adjacent identifier is not
/// mistaken for the standalone `BEGIN` keyword. See .
fn is_identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b > 127
}

/// Checks whether `target` keyword matches in `bytes` at exactly `idx`.
/// Returns the index just past the keyword on a match, or `None`.
/// This is O(1): it performs no whitespace or trivia skipping, so it is safe to
/// call at every byte in the scanner loop without an O(N^2) blow-up on
/// whitespace-heavy input. Leading and trailing word-boundary checks (via
/// [`is_identifier_byte`]) prevent matching inside an identifier — `END` inside
/// `dividend`, `BEGIN` inside `xBEGIN`, `END` inside `ENDPOINT`, and (because
/// PostgreSQL allows `$` and non-ASCII letters in identifiers) `BEGIN` inside
/// `x$begin` / `begin$x` or alongside a multibyte identifier character are all
/// rejected. See .
fn match_keyword_at(bytes: &[u8], idx: usize, target: &str) -> Option<usize> {
    // Leading boundary: the previous byte must not be an identifier byte.
    if idx > 0 && is_identifier_byte(bytes[idx - 1]) {
        return None;
    }
    let target_len = target.len();
    if idx + target_len > bytes.len() {
        return None;
    }
    if bytes[idx..idx + target_len].eq_ignore_ascii_case(target.as_bytes()) {
        // Trailing boundary: the next byte must not be an identifier byte.
        let end = idx + target_len;
        if end < bytes.len() && is_identifier_byte(bytes[end]) {
            return None;
        }
        Some(end)
    } else {
        None
    }
}

/// Skips SQL trivia (ASCII whitespace, `--` line comments, and nestable
/// `/* ... */` block comments) starting at `idx`, then checks whether `target`
/// keyword matches at the first significant byte.
/// Returns the index just past the keyword on a match, or `None`.
/// This is O(N) in the run of trivia it skips, so it is only safe to call
/// once per keyword match — never per-byte in the main loop. It backs the
/// second-keyword peek after a `BEGIN` match (checking for `ATOMIC`), where the
/// trivia run is bounded by the comment/whitespace between two keywords. The
/// trivia rules mirror [`skip_sql_trivia`]; `BEGIN /* x */ ATOMIC` and
/// `BEGIN -- x\nATOMIC` therefore open a compound-statement block as the spec
/// requires.
fn skip_whitespace_and_match(bytes: &[u8], idx: usize, target: &str) -> Option<usize> {
    let mut i = idx;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        // Line comment: `--` through end of line.
        if bytes.get(i) == Some(&b'-') && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment: `/* ... */`, nestable to match `skip_sql_trivia`.
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes.get(i) == Some(&b'*') && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        break;
    }
    match_keyword_at(bytes, i, target)
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
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]` (under
    /// `tests/` the attribute requires a paired `// JUSTIFICATION (djogi#<n>):`
    /// comment, validated by `cargo xtask check-justifications`). See the
    /// [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Prefer the typed surface — `Model::objects.filter(...).fetch_all(ctx)`
    /// for any predicate the queryset can express. Reach for `raw_query`
    /// only for shapes the typed layer cannot describe today (recursive CTEs,
    /// set-returning functions, bespoke joins).
    /// `T: FromPgRow` decodes positionally against the wire row, so the
    /// `SELECT` projection list must match the model's column order. The
    /// canonical order is `id, created_at, updated_at, ...user_fields` for
    /// `#[model]`-derived structs; ad-hoc rowtypes implement
    /// [`FromPgRow`](crate::pg::decode::FromPgRow) with whatever shape they
    /// need.
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#234): typed surface lacks recursive CTE.
    /// async fn ancestor_threads(ctx: &mut DjogiContext, root_id: HeerId)
    /// -> djogi::Result<Vec<Comment>>
    /// {
    /// ctx.raw_query(
    /// "WITH RECURSIVE ancestors AS (
    /// SELECT * FROM comments WHERE id = $1
    /// UNION ALL
    /// SELECT c.* FROM comments c
    /// JOIN ancestors a ON c.id = a.parent_id
    /// )
    /// SELECT id, created_at, updated_at, parent_id, body
    /// FROM ancestors",
    /// &[&root_id],
    /// ).await
    /// }
    /// ```
    async fn raw_query<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>, DjogiError>;

    /// Run a raw `SELECT` and return undecoded `tokio_postgres::Row` values.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Prefer the typed surface — `Model::objects.filter(...).fetch_all(ctx)`
    /// for any predicate the queryset can express. If the typed surface
    /// cannot describe the shape but the row decodes into a `FromPgRow`,
    /// prefer [`raw_query`](RawAccessExtBase::raw_query) over `raw_rows` so
    /// the per-row decode is positional and debug-asserted. Reach for
    /// `raw_rows` only when the caller really does need to inspect column
    /// metadata or call [`tokio_postgres::Row::try_get`] on heterogenous
    /// columns by name (e.g. dynamic introspection helpers).
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Prefer [`raw_query`](RawAccessExtBase::raw_query) when the row shape
    /// fits a `FromPgRow` impl — the per-row decode is positional and
    /// debug-asserted. Reach for `raw_rows` only when the caller really does
    /// need to inspect column metadata or call [`tokio_postgres::Row::try_get`]
    /// on heterogenous columns by name (e.g. dynamic introspection helpers).
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#456): introspecting column metadata for the admin
    /// // schema diff renderer; FromPgRow does not expose column types.
    /// async fn dump_columns(ctx: &mut DjogiContext, table: &str)
    /// -> djogi::Result<Vec<tokio_postgres::Row>>
    /// {
    /// ctx.raw_rows(
    /// "SELECT column_name, data_type FROM information_schema.columns
    /// WHERE table_name = $1",
    /// &[&table],
    /// ).await
    /// }
    /// ```
    async fn raw_rows(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError>;

    /// Run a raw `SELECT` expected to return exactly one row, decoded into `T`.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Returns [`DjogiError::NotFound`] when the query produces zero rows;
    /// the framework does not enforce the upper bound, so the caller is
    /// responsible for using `LIMIT 1` (or otherwise guaranteeing
    /// uniqueness) when required. Prefer
    /// [`Model::get`](crate::model::Model::get) /
    /// `QuerySet::fetch_one` for typed-surface lookups.
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#789): typed surface lacks JSON-aggregated reads.
    /// async fn fetch_one_summary(ctx: &mut DjogiContext, id: HeerId)
    /// -> djogi::Result<UserSummary>
    /// {
    /// ctx.raw_fetch_one(
    /// "SELECT id, jsonb_build_object('posts', count(p.id)) AS summary
    /// FROM users u LEFT JOIN posts p ON p.author_id = u.id
    /// WHERE u.id = $1 GROUP BY u.id LIMIT 1",
    /// &[&id],
    /// ).await
    /// }
    /// ```
    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError>;

    /// Run a raw `SELECT` and return the first column of the first row as a
    /// scalar value of type `T`.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Returns [`DjogiError::NotFound`] when the query produces zero rows.
    /// Use for `SELECT COUNT(*)`, `SELECT MAX(...)`, and similar single-value
    /// reductions. Prefer the queryset's `.count(ctx)` / `.exists(ctx)` /
    /// aggregate-projection terminals when the typed surface covers the
    /// shape.
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#101): summary aggregate the visage layer doesn't
    /// // yet project for nested JSONB facets.
    /// async fn open_invoices_total_cents(ctx: &mut DjogiContext)
    /// -> djogi::Result<i64>
    /// {
    /// ctx.raw_scalar(
    /// "SELECT COALESCE(SUM(total_cents), 0)
    /// FROM invoices WHERE status = 'open'",
    /// &[],
    /// ).await
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
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Prefer `Model::create` / `Model::save` / `Model::delete` for single-row
    /// CRUD and `QuerySet::update` / `QuerySet::delete` for bulk writes. Reach
    /// for `raw_execute` only for shapes the typed layer cannot express today
    /// (e.g. preserving `updated_at` across a bulk update — the queryset
    /// always stamps it).
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#202): bulk update must preserve updated_at; the
    /// // queryset bulk-update path always stamps `updated_at = now`.
    /// async fn restamp_recent(ctx: &mut DjogiContext, days: i32)
    /// -> djogi::Result<u64>
    /// {
    /// ctx.raw_execute(
    /// "UPDATE posts SET view_count = view_count + 1
    /// WHERE created_at > now - $1::interval",
    /// &[&format!("{days} days")],
    /// ).await
    /// }
    /// ```
    async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError>;

    /// Run a raw DDL batch (one or more semicolon-separated statements,
    /// no parameters).
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// `raw_ddl` is `batch_execute(sql)` under a friendlier name — it
    /// carries the same blast radius as [`raw_execute`](RawAccessExtBase::raw_execute)
    /// and intentionally does not project through the migration substrate.
    /// When the context is already transaction-backed, Djogi preflights each
    /// top-level statement in the batch and rejects session-scoped statement
    /// heads (`SET`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`, `PREPARE`,
    /// `DEALLOCATE`) with [`DjogiError::SessionStatementDisallowedInTransaction`]
    /// and transaction-control statements (`BEGIN` — but not `BEGIN ATOMIC`,
    /// a compound-statement delimiter — `COMMIT`, `ROLLBACK`, `END`, `ABORT`,
    /// `SAVEPOINT`, `RELEASE`) with
    /// [`DjogiError::RawTransactionControlDisallowedInTransaction`] before SQL
    /// reaches Postgres. The scanner respects comments, string literals,
    /// dollar-quoted bodies, and `BEGIN ATOMIC ... END` compound-statement
    /// boundaries; it is not a naive `split(';')`.
    /// Tests that need to set up tables MUST use
    /// `#[djogi::djogi_test(sync_models = [...])]` instead — `sync_models`
    /// projects through the descriptor / `pk_default_sql` pipeline so
    /// projection bugs surface from the test surface (tracking issue
    /// ).
    /// Reach for `raw_ddl` only for setup that cannot live in a model
    /// descriptor (`CREATE EXTENSION`, custom types declared outside djogi's
    /// schema-snapshot model, role / permission grants).
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#303): PostGIS extension install runs once per
    /// // database; the future #[djogi_test(extensions = ["postgis"])] surface
    /// // This surface is the preferred path once implemented.
    /// async fn install_postgis(ctx: &mut DjogiContext) -> djogi::Result<> {
    /// ctx.raw_ddl("CREATE EXTENSION IF NOT EXISTS postgis").await
    /// }
    /// ```
    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError>;

    /// Open a server-side cursor and yield rows lazily as a
    /// [`RawCursorStream`].
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Postgres cursors are transaction-local — the surrounding context MUST
    /// be transaction-backed. Calling `raw_stream` on a pool-backed context
    /// returns [`DjogiError::StreamOutsideTransaction`] at construction time
    /// (not at the first `poll_next`), so the misuse surfaces immediately.
    /// Wrap the consumer in `atomic(&mut ctx, |tx| Box::pin(async move {
    /// ... }))` so the `tx` argument is transaction-backed.
    /// Uses the framework default fetch size (chunk-size for the
    /// `FETCH FORWARD` calls under the cursor). For control over the chunk
    /// shape, use [`raw_stream_with_fetch_size`](RawAccessExtBase::raw_stream_with_fetch_size).
    /// Prefer `QuerySet::stream(ctx)` for typed-surface streaming.
    /// ```ignore
    /// use djogi::prelude::*;
    /// use futures::StreamExt;
    ///
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#404): export job needs server-side cursor; the
    /// // typed QuerySet::stream is preferred when the shape fits.
    /// async fn export_orders(ctx: &mut DjogiContext) -> djogi::Result<> {
    /// atomic(ctx, |tx| Box::pin(async move {
    /// let mut stream = tx.raw_stream(
    /// "SELECT id, total_cents FROM orders ORDER BY id",
    /// &[],
    /// ).await?;
    /// while let Some(row) = stream.next.await {
    /// let _row = row?; // process row
    /// }
    /// Ok()
    /// })).await
    /// }
    /// ```
    async fn raw_stream<'ctx>(
        &'ctx mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<RawCursorStream<'ctx>, DjogiError>;

    /// Same as [`raw_stream`](RawAccessExtBase::raw_stream) but caller-tunable
    /// `FETCH FORWARD` chunk size.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Prefer the typed surface — `Model::objects` / `QuerySet::stream`
    /// inside an `atomic(...)` scope — for any shape the typed layer can
    /// describe. Reach for `raw_stream_with_fetch_size` only when the typed
    /// stream cannot describe the projection AND the default chunk size used
    /// by `raw_stream` is the wrong shape for the consumer (typically very
    /// large exports or very latency-sensitive previews).
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// `fetch_size` of `0` returns [`DjogiError::Validation`] — the cursor
    /// driver cannot make progress on an empty fetch chunk. Larger values
    /// reduce round-trips at the cost of per-chunk memory; smaller values
    /// reduce latency to the first row at the cost of more network round
    /// trips. The framework default (used by `raw_stream`) is a balanced
    /// middle ground.
    /// Same transaction-context rules as [`raw_stream`](RawAccessExtBase::raw_stream):
    /// pool-backed contexts return [`DjogiError::StreamOutsideTransaction`]
    /// at construction time.
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#505): export job tunes chunk size to match the
    /// // downstream consumer's batch boundary.
    /// async fn export_orders_chunked(ctx: &mut DjogiContext) -> djogi::Result<> {
    /// atomic(ctx, |tx| Box::pin(async move {
    /// let mut stream = tx.raw_stream_with_fetch_size(
    /// "SELECT id, total_cents FROM orders ORDER BY id",
    /// &[],
    /// 100, // fetch 100 rows per round-trip
    /// ).await?;
    /// use futures::StreamExt;
    /// while let Some(row) = stream.next.await {
    /// let _row = row?;
    /// }
    /// Ok()
    /// })).await
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
        reject_transaction_backed_sql(self, sql)?;
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
        reject_transaction_backed_sql(self, sql)?;
        // No post-query decode — the existing `query_all` guard already
        // covers the only Err/cancel exit shape.
        self.__query_all_for_macros(sql, params).await
    }

    async fn raw_fetch_one<T: FromPgRow>(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<T, DjogiError> {
        reject_transaction_backed_sql(self, sql)?;
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
        reject_transaction_backed_sql(self, sql)?;
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
        reject_transaction_backed_sql(self, sql)?;
        self.__execute_for_macros(sql, params).await
    }

    async fn raw_ddl(&mut self, sql: &str) -> Result<(), DjogiError> {
        reject_transaction_backed_sql_batch(self, sql)?;
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
/// Base trait: no `Send` bound. The generated [`RawPoolAccessExt`] variant
/// adds `Send` bounds to the future returned by `raw_with_client`. Reaching
/// any method here is djogi's `unsafe`-equivalent — see the
/// [module docs](self) and the
/// [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
#[doc(hidden)]
#[trait_variant::make(RawPoolAccessExt: Send)]
pub trait RawPoolAccessExtBase: sealed::Sealed {
    /// Borrow the underlying [`DjogiPool`] when the context is pool-backed.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Returns `None` when the context is transaction-backed — pool reads
    /// during a transaction would route around the surrounding scope. Use
    /// for pool-state introspection (capacity, idle counts) when wiring
    /// adopter-side metrics; otherwise prefer the typed surface
    /// (`DjogiContext::from_pool` for fresh handles, `share_pool` to clone
    /// the inner `Arc`).
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#606): pool-state introspection for adopter
    /// // metrics; the typed surface does not yet expose pool-stats reads.
    /// fn pool_status(ctx: &DjogiContext) -> Option<usize> {
    /// ctx.raw_pool.map(|p| p.status.available)
    /// }
    /// ```
    fn raw_pool(&self) -> Option<&DjogiPool>;

    /// Borrow the underlying [`PgConnection`] when the context is
    /// transaction-backed.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Reaching this is djogi's `unsafe`-equivalent — every call must walk
    /// through `#[djogi::deliberately_bypass_convention_with_raw_sql]`. See
    /// the [Raw SQL escape hatches spec](https://github.com/tarunvir/djogi/blob/main/docs/spec/raw-sql-escape-hatches.md).
    /// Returns `None` when the context is pool-backed — there is no
    /// long-lived connection to borrow — and also when a transaction-backed
    /// context has been poisoned by a nested `atomic` cancellation. Use for
    /// connection-state inspection (savepoint depth, in-progress transaction
    /// state) when an adopter-side helper needs to branch on the inner state.
    /// Prefer
    /// [`DjogiContext::savepoint_depth`](crate::DjogiContext::savepoint_depth)
    /// and the typed transaction substrate for ordinary use.
    /// ```ignore
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#707): transaction-state inspection for a custom
    /// // tracing layer.
    /// fn debug_conn(ctx: &mut DjogiContext) -> bool {
    /// ctx.raw_conn.is_some
    /// }
    /// ```
    fn raw_conn(&mut self) -> Option<&mut PgConnection>;

    /// Run a closure with a checked-out raw [`tokio_postgres::Client`] from
    /// the underlying pool.
    /// Raw escape hatch — djogi's `unsafe`-equivalent. See the
    /// [module docs](self) for the bypass-attribute convention.
    /// Prefer the typed surface — `Model::objects` / `QuerySet`,
    /// `Model::create` / `save` / `delete`, and `djogi::transaction::atomic`
    /// for routine reads, writes, and transactions. `raw_with_client` is
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
    /// Returns [`DjogiError::Db`] wrapping the underlying transport / pool
    /// error when the context has no pool to draw from (pure transaction-
    /// scoped contexts cannot satisfy `raw_with_client`).
    /// See the [connection-pool guide](https://github.com/tarunvir/djogi/blob/main/docs/guide/pool.md#raw-client-escape-hatch--raw_with_client)
    /// for the canonical treatment of when to reach for this surface.
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// #[djogi::deliberately_bypass_convention_with_raw_sql]
    /// // JUSTIFICATION (djogi#808): COPY IN ingest needs binary protocol; the
    /// // typed surface has no streaming-bulk-insert primitive yet.
    /// async fn copy_in_orders(pool: &DjogiPool) -> djogi::Result<> {
    /// pool.raw_with_client(|client| Box::pin(async move {
    /// let _sink = client.copy_in("COPY orders FROM STDIN").await?;
    /// // write rows to the sink ...
    /// Ok()
    /// })).await
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
        TransactionBackedRawSqlRefusal, classify_raw_ddl_transaction_backed_refusal,
        classify_raw_ddl_transaction_session_statement, classify_transaction_backed_refusal,
        classify_transaction_control_statement, classify_transaction_session_statement,
    };
    use crate::DjogiError;

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
    fn classify_raw_ddl_transaction_session_statement_handles_utf8_inside_dollar_quote() {
        let sql = r#"
            DO $body$
            BEGIN
                -- Unicode comment inside the body: bootstrap — extensions
                PERFORM 1;
            END
            $body$;
            CREATE TEMP TABLE djogi_282_classifier_utf8_ok (value integer);
        "#;

        assert_eq!(classify_raw_ddl_transaction_session_statement(sql), None);
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

    #[test]
    fn classify_raw_ddl_transaction_session_statement_rejects_session_set_in_batch() {
        let sql = r#"
            CREATE TEMP TABLE djogi_282_classifier_set_rejected (value integer);
            SET statement_timeout = '1ms';
        "#;

        assert_eq!(
            classify_raw_ddl_transaction_session_statement(sql),
            Some("SET")
        );
    }

    // -----------------------------------------------------------------------
    // Transaction control statement classification (#306).
    // -----------------------------------------------------------------------

    #[test]
    fn classify_transaction_control_statement_detects_all_nine_forms() {
        assert_eq!(
            classify_transaction_control_statement("BEGIN"),
            Some("BEGIN")
        );
        assert_eq!(
            classify_transaction_control_statement("START TRANSACTION"),
            Some("START TRANSACTION")
        );
        assert_eq!(
            classify_transaction_control_statement("COMMIT"),
            Some("COMMIT")
        );
        assert_eq!(
            classify_transaction_control_statement("ROLLBACK"),
            Some("ROLLBACK")
        );
        assert_eq!(classify_transaction_control_statement("END"), Some("END"));
        assert_eq!(
            classify_transaction_control_statement("ABORT"),
            Some("ABORT")
        );
        assert_eq!(
            classify_transaction_control_statement("SAVEPOINT my_sp"),
            Some("SAVEPOINT")
        );
        assert_eq!(
            classify_transaction_control_statement("RELEASE SAVEPOINT my_sp"),
            Some("RELEASE")
        );
        assert_eq!(
            classify_transaction_control_statement("RELEASE"),
            Some("RELEASE")
        );
        assert_eq!(
            classify_transaction_control_statement("ROLLBACK TO my_sp"),
            Some("ROLLBACK")
        );
        assert_eq!(
            classify_transaction_control_statement("ROLLBACK WORK TO my_sp"),
            Some("ROLLBACK")
        );
        assert_eq!(
            classify_transaction_control_statement("ROLLBACK TRANSACTION TO my_sp"),
            Some("ROLLBACK")
        );
        assert_eq!(
            classify_transaction_control_statement("ROLLBACK WORK"),
            Some("ROLLBACK")
        );
        assert_eq!(
            classify_transaction_control_statement("ROLLBACK TRANSACTION"),
            Some("ROLLBACK")
        );
        assert_eq!(
            classify_transaction_control_statement("COMMIT WORK"),
            Some("COMMIT")
        );
        assert_eq!(
            classify_transaction_control_statement("COMMIT TRANSACTION"),
            Some("COMMIT")
        );
        assert_eq!(
            classify_transaction_control_statement("END WORK"),
            Some("END")
        );
        assert_eq!(
            classify_transaction_control_statement("END TRANSACTION"),
            Some("END")
        );
    }

    #[test]
    fn classify_transaction_control_statement_is_case_insensitive() {
        for sql in ["commit", "CoMmIt", "COMMIT"] {
            assert_eq!(
                classify_transaction_control_statement(sql),
                Some("COMMIT"),
                "expected COMMIT for {sql:?}"
            );
        }
        for sql in ["begin", "BeGiN", "BEGIN"] {
            assert_eq!(
                classify_transaction_control_statement(sql),
                Some("BEGIN"),
                "expected BEGIN for {sql:?}"
            );
        }
        for sql in [
            "start transaction",
            "START TRANSACTION",
            "Start Transaction",
        ] {
            assert_eq!(
                classify_transaction_control_statement(sql),
                Some("START TRANSACTION"),
                "expected START TRANSACTION for {sql:?}"
            );
        }
    }

    #[test]
    fn classify_transaction_control_statement_handles_leading_trivia() {
        assert_eq!(
            classify_transaction_control_statement("  COMMIT"),
            Some("COMMIT")
        );
        assert_eq!(
            classify_transaction_control_statement("  \n  \t  rollback"),
            Some("ROLLBACK")
        );
        assert_eq!(
            classify_transaction_control_statement("-- line comment\nCOMMIT"),
            Some("COMMIT")
        );
        assert_eq!(
            classify_transaction_control_statement("/* block */ BEGIN"),
            Some("BEGIN")
        );
    }

    #[test]
    fn classify_transaction_control_statement_returns_none_for_non_transaction_sql() {
        for sql in [
            "SELECT 1",
            "INSERT INTO users (name) VALUES ('test')",
            "UPDATE posts SET title = 'x'",
            "DELETE FROM comments WHERE id = 1",
            "CREATE TABLE foo (id bigint)",
            "SET LOCAL statement_timeout = '5s'",
            "SET CONSTRAINTS ALL IMMEDIATE",
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        ] {
            assert_eq!(
                classify_transaction_control_statement(sql),
                None,
                "expected None for non-transaction SQL: {sql:?}"
            );
        }
    }

    #[test]
    fn classify_transaction_backed_refusal_prioritizes_transaction_control_over_session() {
        // COMMIT is transaction control, not session — should be TransactionControl
        let refusal = classify_transaction_backed_refusal("COMMIT").expect("expected refusal");
        match refusal {
            TransactionBackedRawSqlRefusal::TransactionControl(s) => {
                assert_eq!(s, "COMMIT");
            }
            _ => panic!("expected TransactionControl(COMMIT), got {:?}", refusal),
        }
    }

    #[test]
    fn classify_transaction_backed_refusal_wraps_session_statements() {
        let refusal = classify_transaction_backed_refusal("RESET ALL").expect("expected refusal");
        match refusal {
            TransactionBackedRawSqlRefusal::SessionStatement(s) => {
                assert_eq!(s, "RESET");
            }
            _ => panic!("expected SessionStatement(RESET), got {:?}", refusal),
        }
    }

    #[test]
    fn classify_transaction_backed_refusal_into_error_produces_correct_variant() {
        let refusal = classify_transaction_backed_refusal("COMMIT").expect("expected refusal");
        let err = refusal.into_error();
        // Verify the error variant by matching on it
        match err {
            DjogiError::RawTransactionControlDisallowedInTransaction { statement } => {
                assert_eq!(statement, "COMMIT");
            }
            _ => panic!(
                "expected RawTransactionControlDisallowedInTransaction, got {:?}",
                err
            ),
        }

        let refusal = classify_transaction_backed_refusal("LISTEN foo").expect("expected refusal");
        let err = refusal.into_error();
        match err {
            DjogiError::SessionStatementDisallowedInTransaction { statement } => {
                assert_eq!(statement, "LISTEN");
            }
            _ => panic!(
                "expected SessionStatementDisallowedInTransaction, got {:?}",
                err
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Batch scanner tests for unified refusal (#306).
    // -----------------------------------------------------------------------

    #[test]
    fn classify_raw_ddl_batch_ignores_transaction_keywords_in_dollar_quoted_body() {
        let sql = r#"
            DO $body$
            BEGIN
                PERFORM 'COMMIT should be ignored here';
                PERFORM $$nested ROLLBACK$$;
            END
            $body$;
            SELECT 1;
        "#;
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_detects_transaction_control_after_safe_ddl() {
        let sql = r#"
            CREATE TEMP TABLE foo (value integer);
            COMMIT;
        "#;
        match classify_raw_ddl_transaction_backed_refusal(sql) {
            Some(TransactionBackedRawSqlRefusal::TransactionControl(s)) => {
                assert_eq!(s, "COMMIT");
            }
            _ => panic!("expected TransactionControl(COMMIT)"),
        }
    }

    #[test]
    fn classify_raw_ddl_batch_detects_session_statement_after_safe_ddl() {
        let sql = r#"
            CREATE TEMP TABLE foo (value integer);
            RESET ALL;
        "#;
        match classify_raw_ddl_transaction_backed_refusal(sql) {
            Some(TransactionBackedRawSqlRefusal::SessionStatement(s)) => {
                assert_eq!(s, "RESET");
            }
            _ => panic!("expected SessionStatement(RESET)"),
        }
    }

    #[test]
    fn classify_raw_ddl_batch_allows_trivia_only_and_safe_batches() {
        assert_eq!(
            classify_raw_ddl_transaction_backed_refusal(
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
            CREATE TEMP TABLE djogi_306_classifier_ok (value integer);
        "#;
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_transaction_control_statement_start_without_transaction_is_none() {
        // "START" alone is not transaction control — requires "TRANSACTION" as second word
        assert_eq!(classify_transaction_control_statement("START"), None);
        assert_eq!(classify_transaction_control_statement("START ALL"), None);
    }

    #[test]
    fn classify_transaction_backed_refusal_returns_none_for_safe_sql() {
        assert_eq!(classify_transaction_backed_refusal("SELECT 1"), None);
        assert_eq!(
            classify_transaction_backed_refusal("INSERT INTO t VALUES (1)"),
            None
        );
    }

    // -----------------------------------------------------------------------
    // BEGIN ATOMIC ... END compound-statement scanning.
    // The raw_ddl batch scanner must treat `BEGIN ATOMIC ... END` as a single
    // compound statement: internal semicolons do not split, and the closing
    // END is not transaction control. CASE ... END nesting inside the block,
    // string / comment / quote contexts, and word boundaries must all be
    // handled without falsely opening or closing a block.
    // -----------------------------------------------------------------------

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_block_allowed() {
        // BEGIN ATOMIC ... END is valid SQL-standard compound statement syntax.
        // The scanner must not classify the closing END as transaction control,
        // nor the head BEGIN as transaction control (BEGIN ATOMIC != bare BEGIN).
        let sql = "BEGIN ATOMIC SELECT 1; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_create_function_then_atomic_block_allowed() {
        // Realistic migration shape: CREATE FUNCTION followed by atomic compound.
        let sql = r#"
            CREATE FUNCTION f() RETURNS integer AS $$ SELECT 1; END $$ LANGUAGE sql;
            BEGIN ATOMIC SELECT 2; END;
            SELECT 3;
        "#;
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_case_inside_atomic_allowed() {
        // CASE ... END inside an atomic block must not prematurely close it.
        let sql = "BEGIN ATOMIC SELECT CASE WHEN x > 0 THEN 'pos' ELSE 'neg' END; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_nested_case_inside_atomic_allowed() {
        let sql = "BEGIN ATOMIC SELECT CASE WHEN x > 0 THEN CASE WHEN y > 0 THEN 1 ELSE 0 END ELSE -1 END; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_multiple_cases_inside_atomic_allowed() {
        let sql = "BEGIN ATOMIC SELECT CASE WHEN a THEN 1 END; SELECT CASE WHEN b THEN 2 END; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_case_in_atomic_then_commit_detected() {
        // Regression: double-counting CASE depth would leave the atomic block
        // open, swallowing the trailing COMMIT. CASE advances idx (counted
        // once), so the inner END closes the CASE, the block END closes the
        // atomic block, and the COMMIT is detected.
        let sql = "BEGIN ATOMIC SELECT CASE WHEN a THEN 1 END; END; COMMIT";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_atomic_then_commit_detected() {
        // Transaction control after a (closed) atomic block is still detected.
        let sql = "BEGIN ATOMIC SELECT 1; END; COMMIT";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_endpoint_inside_atomic_block() {
        // ENDPOINT must not match the END keyword (trailing word boundary).
        let sql = "BEGIN ATOMIC SELECT endpoint FROM t; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_nested_begin_atomic_allowed() {
        // Nested BEGIN ATOMIC blocks exercise depth increment / decrement.
        let sql = "BEGIN ATOMIC SELECT 1; BEGIN ATOMIC SELECT 2; END; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_in_line_comment_ignored() {
        // BEGIN ATOMIC inside a line comment at depth 0 must not open a block;
        // the top-level END after the comment is genuine transaction control.
        let sql = "-- BEGIN ATOMIC\nEND;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_in_block_comment_ignored() {
        let sql = "/* BEGIN ATOMIC */ END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_in_single_quote_ignored() {
        let sql = "SELECT 'BEGIN ATOMIC'; END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_in_double_quote_ignored() {
        let sql = "\"BEGIN ATOMIC\"; END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_in_dollar_quote_ignored() {
        let sql = "SELECT $$BEGIN ATOMIC$$; END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_create_function_dollar_quoted_atomic_allowed() {
        // Dollar-quoted CREATE FUNCTION with a BEGIN ATOMIC body (regression
        // guard: existing dollar-quote tracking already makes the body opaque).
        let sql = r#"
            CREATE FUNCTION f() RETURNS integer
                LANGUAGE SQL
                AS $$ BEGIN ATOMIC SELECT 1; END $$;
            SELECT 1;
        "#;
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_bare_begin_still_rejected() {
        // Bare BEGIN (not BEGIN ATOMIC) is still transaction control.
        let sql = "BEGIN; SELECT 1;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_begin_without_atomic_still_rejected() {
        // BEGIN WORK is transaction control; END is also transaction control.
        let sql = "BEGIN WORK; SELECT 1; END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_case_insensitive() {
        let sql = "begin ATOMIC SELECT 1; end";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_begin_atomic_keyword_boundary() {
        // BEGINNATIC is not BEGIN ATOMIC; the trailing END is transaction control.
        let sql = "BEGINNATIC SELECT 1; END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    // --- CLASS B: string / comment / quote contexts inside atomic blocks ---

    #[test]
    fn classify_raw_ddl_batch_string_inside_atomic_block() {
        // 'END' in a string literal inside the block must not decrement depth.
        let sql = "BEGIN ATOMIC SELECT 'END'; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_block_comment_inside_atomic_block() {
        let sql = "BEGIN ATOMIC /* END */ SELECT 1; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_line_comment_inside_atomic_block() {
        // The comment's END appears before an internal semicolon. A premature
        // depth decrement would split there and misclassify — so this shape is
        // discriminating, not masked.
        let sql = "BEGIN ATOMIC SELECT 1 -- END\n; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_double_quote_inside_atomic_block() {
        let sql = r#"BEGIN ATOMIC SELECT "END" FROM t; END"#;
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_dollar_quote_inside_atomic_block() {
        let sql = "BEGIN ATOMIC SELECT $$END$$ FROM t; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_word_boundary_inside_atomic_block() {
        // `dividend` ends in `end` but is a single identifier — leading word
        // boundary prevents a false END match.
        let sql = "BEGIN ATOMIC UPDATE t SET x = dividend; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_leading_word_boundary() {
        // xBEGIN is not BEGIN; the block never opens and the top-level END is
        // genuine transaction control.
        let sql = "xBEGIN ATOMIC SELECT 1; END;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }

    #[test]
    fn classify_raw_ddl_batch_create_function_atomic_body_allowed() {
        // Canonical unquoted shape: a dollar-quoted CREATE FUNCTION body
        // followed by a bare BEGIN ATOMIC compound statement.
        let sql = "CREATE FUNCTION f() RETURNS integer LANGUAGE SQL AS $$ SELECT 1; END $$; BEGIN ATOMIC SELECT 2; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    // --- Classifier two-keyword peek (, Step 2c) ---

    #[test]
    fn classify_transaction_control_statement_begin_atomic_is_none() {
        assert_eq!(
            classify_transaction_control_statement("BEGIN ATOMIC SELECT 1"),
            None
        );
        assert_eq!(
            classify_transaction_control_statement("begin atomic SELECT 1"),
            None
        );
    }

    #[test]
    fn classify_transaction_control_statement_bare_begin_still_detected() {
        assert_eq!(
            classify_transaction_control_statement("BEGIN WORK"),
            Some("BEGIN")
        );
        assert_eq!(
            classify_transaction_control_statement("BEGIN TRANSACTION"),
            Some("BEGIN")
        );
        assert_eq!(
            classify_transaction_control_statement("BEGIN;"),
            Some("BEGIN")
        );
        assert_eq!(
            classify_transaction_control_statement("BEGIN"),
            Some("BEGIN")
        );
    }

    // --- : $ and non-ASCII identifier bytes must not trigger keyword matches ---

    #[test]
    fn classify_raw_ddl_batch_dollar_suffix_begin_not_atomic_opener() {
        // `x$begin atomic` must NOT open an atomic block. If it did,
        // the trailing COMMIT would be swallowed as internal and reach Postgres
        // inside atomic. The COMMIT must be detected as transaction control.
        let sql = "CREATE TEMP TABLE t (x integer); SELECT x$begin atomic FROM t; COMMIT;";
        assert!(
            classify_raw_ddl_transaction_backed_refusal(sql).is_some(),
            "trailing COMMIT after a $-suffixed pseudo-BEGIN must still be refused"
        );
    }

    #[test]
    fn classify_raw_ddl_batch_dollar_prefix_end_does_not_close_atomic_block() {
        // END-site mirror (Task A.4): `x$end` inside a real atomic block must NOT
        // close it. If it closed prematurely, the trailing COMMIT after the real
        // END would be misclassified or the depth would be corrupted. Here the
        // genuine COMMIT after the true block END must be detected.
        let sql = "BEGIN ATOMIC SELECT x$end FROM t; END; COMMIT;";
        assert!(
            classify_raw_ddl_transaction_backed_refusal(sql).is_some(),
            "x$end inside the block must not prematurely close it; trailing COMMIT must be refused"
        );
    }

    #[test]
    fn classify_raw_ddl_batch_begin_dollar_suffix_is_plain_identifier() {
        // Trailing-boundary miss: `begin$x` is a single identifier, not BEGIN.
        // As a standalone statement head it is not transaction control.
        let sql = "SELECT begin$x FROM t;";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_non_ascii_suffix_begin_not_atomic_opener() {
        // Non-ASCII identifier byte before `begin atomic`: a multibyte identifier
        // char (here 'é') makes `begin` part of an identifier, so no block opens
        // and the trailing COMMIT must be refused.
        let sql = "SELECT café_begin atomic FROM t; COMMIT;";
        assert!(
            classify_raw_ddl_transaction_backed_refusal(sql).is_some(),
            "non-ASCII-adjacent pseudo-BEGIN must not open a block; trailing COMMIT must be refused"
        );
    }

    // --- : comments between BEGIN and ATOMIC must still open a block ---

    #[test]
    fn classify_raw_ddl_batch_begin_block_comment_atomic_opens_block() {
        // BEGIN /* c */ ATOMIC must open a compound-statement block (spec: scanner
        // respects comments). The internal COMMIT-looking text and the closing END
        // are then internal, so the batch is allowed.
        let sql = "BEGIN /* c */ ATOMIC SELECT 1; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_begin_line_comment_atomic_opens_block() {
        let sql = "BEGIN -- open the body\n ATOMIC SELECT 1; END";
        assert_eq!(classify_raw_ddl_transaction_backed_refusal(sql), None);
    }

    #[test]
    fn classify_raw_ddl_batch_begin_comment_atomic_then_commit_detected() {
        // Once the comment-separated BEGIN ATOMIC opens and closes, a trailing
        // top-level COMMIT is still detected.
        let sql = "BEGIN /* c */ ATOMIC SELECT 1; END; COMMIT;";
        assert!(classify_raw_ddl_transaction_backed_refusal(sql).is_some());
    }
}
