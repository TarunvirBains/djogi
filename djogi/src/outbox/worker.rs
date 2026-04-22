//! Outbox worker primitives — Phase 5 Task 11.5.
//!
//! These functions implement the consumer side of the transactional outbox
//! pattern. They are gated on the `outbox` cargo feature so adopters who only
//! write to the outbox (via `#[model(events)]`) do not pull in this module.
//!
//! # State machine
//!
//! ```text
//! pending ──claim_pending──► processing ──mark_published──► published
//!                                │
//!                                ├──mark_failed(retryable=true, retry_count < MAX_RETRY)──► pending
//!                                └──mark_failed(retryable=false OR retry_count >= MAX_RETRY)──► failed
//!
//! processing ──recover_stale (lease expired)──► pending
//! ```
//!
//! # Retry budget
//!
//! `MAX_RETRY_COUNT = 10` — if a retryable failure occurs ten or more times the
//! row transitions to `failed` terminally. This is a hard-coded constant rather
//! than a deployment configuration because retry budgets belong in the operator's
//! monitoring + alerting stack, not the framework. If the adopter needs a custom
//! budget they can wrap `mark_failed` in their own relay logic.
//!
//! # Table name validation
//!
//! Every function that accepts an `outbox_table` argument validates it before
//! embedding it in SQL. A valid identifier is: an ASCII letter or underscore as
//! the first byte, followed by ASCII alphanumerics or underscores, up to 63
//! bytes total. Anything that fails this check returns
//! `DjogiError::Db(DbError::other("invalid outbox table name: ..."))`.
//!
//! Validation is done with byte-level checks — no regex engine is used or
//! permitted anywhere in this codebase.

use crate::context::DjogiContext;
use crate::error::DbError;
use crate::{DjogiError, HeerId};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of retryable failures before a row becomes terminally failed.
///
/// Chosen as a round number large enough to absorb transient downstream
/// hiccups without letting poison-pill rows spin forever. Deployments that need
/// more or fewer retries should wrap `mark_failed` in their own relay loop that
/// provides a custom budget.
pub const MAX_RETRY_COUNT: i32 = 10;

// ---------------------------------------------------------------------------
// OutboxRow
// ---------------------------------------------------------------------------

/// A claimed outbox row, returned by [`claim_pending`].
///
/// The fields correspond to the normative `worker_outbox` schema columns. The
/// `id` here is the outbox row's own primary key (not the application row's PK);
/// use `row_id` to correlate with the source table.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    /// Primary key of the outbox row itself. Used by `mark_published` /
    /// `mark_failed` to target the exact row.
    pub id: HeerId,

    /// Primary key of the source application row that triggered this event.
    pub row_id: i64,

    /// The CRUD action that produced this outbox entry. Matches the
    /// `OutboxAction::as_sql_str()` values: `"create"`, `"save"`, or `"delete"`.
    pub action: String,

    /// Full JSONB payload — the serialised source row (after `outbox_exclude`
    /// filtering at write time).
    pub payload: serde_json::Value,

    /// Wall-clock time the outbox row was inserted, in UTC.
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// Identifier validation
// ---------------------------------------------------------------------------

/// Validate that `name` is safe to embed as an unquoted SQL identifier.
///
/// Rules (plain English, no regex):
/// - At least 1 byte, at most 63 bytes (Postgres identifier limit).
/// - First byte must be an ASCII letter (`a`..`z` or `A`..`Z`) or underscore.
/// - All subsequent bytes must be ASCII alphanumeric (`a`..`z`, `A`..`Z`,
///   `0`..`9`) or underscore.
///
/// Returns `Ok(())` on success, `Err(...)` with a descriptive message on
/// failure. The caller embeds the name in SQL only after this check passes.
fn validate_table_ident(name: &str) -> Result<(), DjogiError> {
    let bytes = name.as_bytes();

    if bytes.is_empty() || bytes.len() > 63 {
        return Err(DjogiError::Db(DbError::other(format!(
            "invalid outbox table name {:?}: must be 1–63 bytes",
            name
        ))));
    }

    // First byte: ASCII letter or underscore.
    let first = bytes[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return Err(DjogiError::Db(DbError::other(format!(
            "invalid outbox table name {:?}: first character must be an ASCII letter or underscore",
            name
        ))));
    }

    // Remaining bytes: ASCII alphanumeric or underscore.
    for &b in &bytes[1..] {
        if !b.is_ascii_alphanumeric() && b != b'_' {
            return Err(DjogiError::Db(DbError::other(format!(
                "invalid outbox table name {:?}: contains disallowed character '{}'",
                name, b as char
            ))));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Worker primitives
// ---------------------------------------------------------------------------

/// Atomically claim up to `batch_size` pending outbox rows and return them.
///
/// Uses `FOR UPDATE SKIP LOCKED` inside a sub-`SELECT` so multiple concurrent
/// worker processes claim disjoint batches without coordination overhead.
/// Each claimed row transitions `pending → processing` and its `leased_until`
/// is set to `now() + lease_duration`, giving the caller a window in which to
/// publish before [`recover_stale`] can reclaim the row.
///
/// The claim is atomic: if the enclosing transaction rolls back, the rows
/// remain `pending`. Callers should therefore wrap their claim + publish cycle
/// in `atomic()` so a publish failure or crash does not leave orphaned
/// `processing` rows.
///
/// # Parameters
///
/// - `outbox_table` — the bare table name (e.g. `"worker_outbox"`). Validated
///   before SQL embedding; returns an error if invalid.
/// - `batch_size` — maximum number of rows to claim in one call.
/// - `lease_duration` — how long the claimed rows are locked. Pass a value
///   long enough to cover the publish round-trip plus reasonable retry padding.
pub async fn claim_pending(
    ctx: &mut DjogiContext,
    outbox_table: &str,
    batch_size: u32,
    lease_duration: time::Duration,
) -> Result<Vec<OutboxRow>, DjogiError> {
    validate_table_ident(outbox_table)?;

    // `time::Duration` does not implement `postgres_types::ToSql` directly
    // (the `with-time-0_3` feature covers timestamps, not intervals). We embed
    // the whole-second count directly into the SQL string as an integer literal
    // so Postgres can parse it at planning time without any bind-parameter type
    // ambiguity. The value comes from `whole_seconds()` on a validated
    // `time::Duration`, not from user input, so direct embedding is safe.
    // `leased_until IS NULL OR leased_until <= now()` gates pending rows that
    // are still inside a retry backoff window (see `mark_failed`). Freshly
    // inserted rows have `leased_until = NULL` and claim immediately; rows that
    // were re-queued after a retryable failure have a `leased_until` in the
    // future that encodes their next-attempt time.
    let lease_secs = lease_duration.whole_seconds();
    let sql = format!(
        "UPDATE {outbox_table} \
         SET state = 'processing', leased_until = now() + make_interval(secs => {lease_secs}) \
         WHERE id IN ( \
            SELECT id FROM {outbox_table} \
            WHERE state = 'pending' \
              AND (leased_until IS NULL OR leased_until <= now()) \
            ORDER BY created_at \
            LIMIT $1 \
            FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING id, row_id, action, payload, created_at"
    );

    let batch_size_i64 = batch_size as i64;
    let params: &[&(dyn postgres_types::ToSql + Sync)] = &[&batch_size_i64];
    let rows = ctx.query_all(&sql, params).await?;

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let id_raw: i64 = row.try_get(0).map_err(|e| {
            DjogiError::Db(DbError::other(format!("claim_pending: decode id: {e}")))
        })?;
        let row_id: i64 = row.try_get(1).map_err(|e| {
            DjogiError::Db(DbError::other(format!("claim_pending: decode row_id: {e}")))
        })?;
        let action: String = row.try_get(2).map_err(|e| {
            DjogiError::Db(DbError::other(format!("claim_pending: decode action: {e}")))
        })?;
        let payload: serde_json::Value = row.try_get(3).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "claim_pending: decode payload: {e}"
            )))
        })?;
        let created_at: OffsetDateTime = row.try_get(4).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "claim_pending: decode created_at: {e}"
            )))
        })?;

        let id = HeerId::from_i64(id_raw).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "claim_pending: invalid HeerId value {id_raw}: {e}"
            )))
        })?;

        result.push(OutboxRow {
            id,
            row_id,
            action,
            payload,
            created_at,
        });
    }

    Ok(result)
}

/// Transition an outbox row from `processing` to `published` (terminal success).
///
/// After this call the row will never be claimed again. Callers should invoke
/// this only after the downstream publisher has confirmed delivery.
///
/// `row_id` here is the `id` column of the **outbox** row (the value in
/// `OutboxRow::id`), not the source application row's PK.
pub async fn mark_published(
    ctx: &mut DjogiContext,
    outbox_table: &str,
    row_id: HeerId,
) -> Result<(), DjogiError> {
    validate_table_ident(outbox_table)?;

    let sql = format!(
        "UPDATE {outbox_table} \
         SET state = 'published' \
         WHERE id = $1 AND state = 'processing'"
    );

    let id_raw = row_id.as_i64();
    let params: &[&(dyn postgres_types::ToSql + Sync)] = &[&id_raw];
    ctx.execute(&sql, params).await?;
    Ok(())
}

/// Transition an outbox row from `processing` based on publish outcome.
///
/// - **Retryable + below budget**: row moves back to `pending`, `retry_count`
///   increments by one, and `leased_until` is cleared. The next
///   [`claim_pending`] call will pick it up again.
/// - **Non-retryable OR budget exhausted** (`retry_count >= MAX_RETRY_COUNT`):
///   row transitions to `failed` terminally with `failed_reason` set to
///   `error_message`.
///
/// `row_id` is the outbox row's own `id` (from `OutboxRow::id`).
pub async fn mark_failed(
    ctx: &mut DjogiContext,
    outbox_table: &str,
    row_id: HeerId,
    error_message: &str,
    retryable: bool,
) -> Result<(), DjogiError> {
    validate_table_ident(outbox_table)?;

    // First, fetch current retry_count so we can decide which branch to take.
    // We do this as a separate query rather than a single conditional UPDATE to
    // keep the SQL readable. The row is still in state='processing' so no
    // other worker can claim it during this window (they skip 'processing').
    let fetch_sql =
        format!("SELECT retry_count FROM {outbox_table} WHERE id = $1 AND state = 'processing'");
    let id_raw = row_id.as_i64();
    let fetch_params: &[&(dyn postgres_types::ToSql + Sync)] = &[&id_raw];
    let maybe_row = ctx.query_opt(&fetch_sql, fetch_params).await?;

    let current_retry: i32 = match maybe_row {
        Some(row) => row.try_get(0).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "mark_failed: decode retry_count: {e}"
            )))
        })?,
        None => {
            // Row not found or not in 'processing' state — nothing to do.
            return Ok(());
        }
    };

    let transition_to_pending = retryable && current_retry < MAX_RETRY_COUNT;

    if transition_to_pending {
        // Retryable and within budget: return to pending, increment counter, and
        // schedule the next attempt with exponential backoff.
        //
        // The backoff reuses `leased_until` as a "not claimable before" gate on
        // pending rows (see `claim_pending`, which filters out pending rows whose
        // `leased_until` is still in the future). This avoids a schema change.
        //
        // Delay = 2^new_retry seconds, capped at 1024 (≈17 min). Base-2 doubling
        // starting at 2s for the first retry, 4s, 8s, … up to the cap at
        // retry_count = 10. Poison-pill rows hit the retry budget before the cap
        // becomes relevant in practice.
        let new_retry = current_retry.saturating_add(1);
        let shift = new_retry.clamp(0, 10) as u32;
        let backoff_secs: i64 = (1i64 << shift).min(1024);

        let sql = format!(
            "UPDATE {outbox_table} \
             SET state = 'pending', retry_count = retry_count + 1, \
                 leased_until = now() + make_interval(secs => {backoff_secs}), \
                 failed_reason = $2 \
             WHERE id = $1 AND state = 'processing'"
        );
        let params: &[&(dyn postgres_types::ToSql + Sync)] = &[&id_raw, &error_message];
        ctx.execute(&sql, params).await?;
    } else {
        // Non-retryable or budget exhausted: mark terminal failure.
        let sql = format!(
            "UPDATE {outbox_table} \
             SET state = 'failed', failed_reason = $2 \
             WHERE id = $1 AND state = 'processing'"
        );
        let params: &[&(dyn postgres_types::ToSql + Sync)] = &[&id_raw, &error_message];
        ctx.execute(&sql, params).await?;
    }

    Ok(())
}

/// Reset stale `processing` rows back to `pending` so they can be reclaimed.
///
/// A row is considered stale when its `leased_until` timestamp is more than
/// `stale_threshold` seconds in the past — i.e. the worker that claimed it
/// failed to call `mark_published` or `mark_failed` within the lease window
/// and the caller-supplied grace threshold has also elapsed. The grace window
/// avoids racing healthy workers whose lease has only just expired. This is
/// the crash-recovery path.
///
/// Returns the number of rows that were moved back to `pending`.
///
/// # When to call this
///
/// Call `recover_stale` periodically (e.g. once per poll loop iteration) from
/// a separate context — not inside the same atomic scope as `claim_pending`.
/// This ensures that rows abandoned by crashed workers are available to the
/// next healthy claim.
pub async fn recover_stale(
    ctx: &mut DjogiContext,
    outbox_table: &str,
    stale_threshold: time::Duration,
) -> Result<u64, DjogiError> {
    validate_table_ident(outbox_table)?;

    // Same interval-embedding approach as claim_pending: embed the whole-second
    // count directly in the SQL string to avoid bind-parameter type ambiguity
    // with Postgres interval arithmetic.
    let threshold_secs = stale_threshold.whole_seconds();
    let sql = format!(
        "UPDATE {outbox_table} \
         SET state = 'pending', leased_until = NULL \
         WHERE state = 'processing' AND leased_until < now() - make_interval(secs => {threshold_secs})"
    );

    let params: &[&(dyn postgres_types::ToSql + Sync)] = &[];
    let count = ctx.execute(&sql, params).await?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Unit tests — identifier validation (no DB required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_idents_pass() {
        assert!(validate_table_ident("worker_outbox").is_ok());
        assert!(validate_table_ident("accounts_outbox").is_ok());
        assert!(validate_table_ident("orders_outbox_v2").is_ok());
        assert!(validate_table_ident("_private").is_ok());
        assert!(validate_table_ident("a").is_ok());
    }

    #[test]
    fn empty_ident_rejected() {
        assert!(validate_table_ident("").is_err());
    }

    #[test]
    fn ident_over_63_bytes_rejected() {
        // 64 lowercase letters
        let long = "a".repeat(64);
        assert!(validate_table_ident(&long).is_err());
    }

    #[test]
    fn ident_exactly_63_bytes_ok() {
        let ok = "a".repeat(63);
        assert!(validate_table_ident(&ok).is_ok());
    }

    #[test]
    fn digit_first_byte_rejected() {
        assert!(validate_table_ident("1outbox").is_err());
    }

    #[test]
    fn hyphen_rejected() {
        assert!(validate_table_ident("worker-outbox").is_err());
    }

    #[test]
    fn dot_rejected() {
        assert!(validate_table_ident("schema.table").is_err());
    }

    #[test]
    fn space_rejected() {
        assert!(validate_table_ident("worker outbox").is_err());
    }
}
