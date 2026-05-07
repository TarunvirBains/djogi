//! Chunked backfill execution for live-plan
//! [`StepKind::BackfillChunked`](crate::live_migrate::plan::StepKind::BackfillChunked)
//! steps.
//!
//! # Per-chunk transaction shape
//!
//! Each chunk runs as its own Postgres transaction. Inside that
//! transaction the runner does three things and commits all three
//! together:
//!
//! 1. `SET LOCAL "djogi.live_migrate.suppress_events" = '1'` — the
//!    session-scoped flag that the future
//!    `#[model(events)]` outbox writer (wired in a later task)
//!    consults before queuing an outbox row. The default is "events
//!    suppressed during a backfill chunk"; opt-in restoration is
//!    available via `--emit-events` or
//!    `#[migration(emit_events_during_backfill)]`, both of which
//!    surface here as `emit_events: bool`. With `emit_events = true`
//!    the runner does NOT issue the `SET LOCAL` and downstream hooks
//!    behave normally.
//! 2. The pattern's `UPDATE … WHERE <idempotent-predicate> LIMIT $1
//!    RETURNING <pk>` query — the actual transformation. Bound count
//!    is the chunk size; the predicate is supplied by the pattern
//!    emitter and is required to be idempotent (re-running the same
//!    predicate against an already-backfilled subrange must produce
//!    no observable change).
//! 3. `UPDATE djogi_live_plans SET backfill_rows_done = …,
//!    last_progress_at = now() WHERE plan_id = $1` — the progress
//!    write.
//!
//! Steps 2 and 3 are committed together. A crash between them would
//! otherwise replay the chunk on resume and double-apply non-idempotent
//! transforms; pinning both writes to the same transaction makes the
//! resume contract safe by construction. See v3 plan §3 lines 411-419.
//!
//! # Idempotent predicate contract
//!
//! The pattern's `WHERE` clause must be idempotent: re-running the same
//! chunk predicate against an already-backfilled subrange must produce
//! no observable change. Patterns that cannot satisfy this classify as
//! `OfflineOnly` rather than masquerading as `ExpandContract` (the
//! classifier rejects pattern definitions whose predicate generator
//! cannot prove idempotency). The runner here trusts the predicate and
//! drives chunks to exhaustion; the idempotency promise is what makes a
//! transient crash between two transactions a replay no-op rather than
//! a double-apply.
//!
//! # Predicate template contract
//!
//! `predicate_template` is an SQL fragment the pattern emitter built —
//! everything from `SET …` through the trailing `RETURNING …` clause,
//! exclusive of the leading `UPDATE <table>`. The runner concatenates
//! `UPDATE <table> ` with the template verbatim and binds `chunk_size`
//! at the `$1` placeholder. The template **must** contain a
//! single `LIMIT $1` placeholder (no other `$n` parameters); the
//! runner does not try to splice a `LIMIT` if one is missing. The
//! pattern emitter owns the SQL; the runner owns transaction shape and
//! resume.
//!
//! # Resume
//!
//! [`resume_backfill`] reads `backfill_rows_done` from
//! `djogi_live_plans`, then drives chunks forward until the predicate
//! exhausts. The per-chunk transaction shape is identical to
//! [`execute_backfill`]; the two functions differ only in whether the
//! starting row count is read from disk.

use crate::__bypass::RawPoolAccessExt as _;
use crate::context::DjogiContext;
use crate::error::{DbError, DjogiError};
use crate::live_migrate::state::{self, PlanStatus};
use crate::transaction::atomic;
use crate::types::HeerId;

/// Custom GUC name used to suppress framework hooks (notably
/// `#[model(events)]` outbox writes) during a backfill chunk's
/// transaction. Set via `SET LOCAL` so the suppression is automatically
/// reset on transaction commit / rollback. This module is the sole
/// producer of the GUC; the consumer (the outbox writer that checks
/// `current_setting(name, true)` before queuing a row) is wired in a
/// later task and will read back through this same const so producer
/// and consumer cannot drift.
pub(crate) const SIDE_EFFECT_SUPPRESSION_TXN_LOCAL: &str = "djogi.live_migrate.suppress_events";

/// Pre-built `set_config(...)` query for the suppression GUC, lazily
/// stitched together from [`SIDE_EFFECT_SUPPRESSION_TXN_LOCAL`] so a
/// rename of the GUC name automatically threads through the SQL.
/// One allocation at first use, then borrowed by every subsequent
/// chunk. The value (`'1'`) is bound via `$1` rather than interpolated.
static SUPPRESS_EVENTS_SET_LOCAL_SQL: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| {
        format!("SELECT set_config('{SIDE_EFFECT_SUPPRESSION_TXN_LOCAL}', $1, true)")
    });

/// A single chunk of backfill work — one transaction. Returned in
/// chunk-ordinal order from [`execute_backfill`] and [`resume_backfill`]
/// so callers (CLI, daemon, tests) can render a per-chunk audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillChunk {
    /// 1-indexed chunk ordinal — also the log line operators see.
    pub ordinal: u64,
    /// Row count this chunk affected (the `UPDATE`'s reported affected
    /// rows). The predicate is exhausted when this drops below
    /// `chunk_size`.
    pub rows_affected: u64,
    /// Cumulative `backfill_rows_done` after this chunk commits. Equal
    /// to the value persisted in `djogi_live_plans.backfill_rows_done`
    /// once the chunk's transaction commits.
    pub rows_done_total: u64,
}

/// Errors raised by [`execute_backfill`] / [`resume_backfill`].
///
/// `#[non_exhaustive]` so future failure modes (e.g. statement-timeout
/// classification, advisory-lock contention) can land without breaking
/// downstream matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackfillError {
    /// The plan_id does not resolve to a row in `djogi_live_plans`.
    /// Surfaces when the runner is asked to drive a backfill against a
    /// plan that was never inserted (or has been hard-deleted).
    #[error("plan {0} not found")]
    PlanNotFound(HeerId),

    /// The plan row exists but its current status is not `Running`.
    /// Backfill execution requires the plan to already be in `running`
    /// — the operator gates step transitions through the CLI
    /// (`djogi live run` / `djogi live resume`); the runner refuses to
    /// auto-promote into `running` on the operator's behalf.
    #[error("plan {0} not in running state (status: {1})")]
    NotRunning(HeerId, &'static str),

    /// The pattern marked its transform non-idempotent. Patterns that
    /// cannot prove an idempotent `WHERE` predicate classify as
    /// `OfflineOnly` rather than reaching this code path; this variant
    /// exists as a fail-closed guard if a future pattern emits a
    /// `BackfillChunked` step without satisfying the contract.
    #[error("predicate must be idempotent — pattern marked transform non-idempotent")]
    NonIdempotentPredicate,

    /// `predicate_template` is malformed — missing the required
    /// `LIMIT $1` placeholder, or carries a different `$n` shape.
    /// Indicates a pattern-emitter bug; the runner refuses rather than
    /// guessing.
    #[error("predicate template missing required `LIMIT $1` placeholder")]
    MalformedPredicateTemplate,

    /// `table` is not a valid unquoted Postgres identifier. The runner
    /// concatenates the table name into SQL directly (parameters can't
    /// be used for table identifiers); a malformed name would either be
    /// a SQL-injection vector or a syntactically broken statement, so
    /// the runner refuses up front.
    #[error("invalid table name {table:?}: {reason}")]
    InvalidTable { table: String, reason: &'static str },

    /// `chunk_size = 0` — would loop forever with zero work. Indicates
    /// a pattern-emitter bug; reject up front.
    #[error("chunk_size must be greater than zero")]
    ZeroChunkSize,

    /// Database / driver error from the underlying connection.
    #[error(transparent)]
    Database(#[from] DjogiError),

    /// The chunk reported zero rows affected before the predicate
    /// exhausted in the conventional `rows_affected < chunk_size` sense.
    /// Strictly speaking this is the same exhaustion signal, but
    /// surfacing it as a distinct outcome catches the pathological case
    /// where a non-idempotent predicate "stuck" on the same rows
    /// across attempts (i.e. the rows are present but the predicate is
    /// no longer matching them) — the operator wants to investigate
    /// rather than spin.
    #[error("backfill chunk produced 0 rows but predicate did not exhaust")]
    PredicateStuck,
}

// ── helpers ───────────────────────────────────────────────────────────

/// Validate a runtime-supplied table name against the Postgres
/// unquoted-identifier contract (ASCII letter or underscore first,
/// alphanumerics or underscores after, ≤ 63 bytes). The runner pastes
/// the table name into SQL directly — parameters cannot be used for
/// identifiers — so the byte-level shape must be checked before the
/// concatenation. Mirrors the rules in `djogi::ident::assert_plain_ident`
/// but returns an error rather than panicking (the table name comes
/// from a plan file, which may have been hand-edited; a panic would be
/// the wrong diagnostic shape for that source).
fn validate_table_ident(table: &str) -> Result<(), BackfillError> {
    let bytes = table.as_bytes();
    if bytes.is_empty() {
        return Err(BackfillError::InvalidTable {
            table: table.to_owned(),
            reason: "table name must not be empty",
        });
    }
    if bytes.len() > 63 {
        return Err(BackfillError::InvalidTable {
            table: table.to_owned(),
            reason: "table name exceeds Postgres's 63-byte identifier limit",
        });
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(BackfillError::InvalidTable {
            table: table.to_owned(),
            reason: "table name must start with an ASCII letter or underscore",
        });
    }
    for &byte in &bytes[1..] {
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(BackfillError::InvalidTable {
                table: table.to_owned(),
                reason: "table name must contain only ASCII alphanumerics and underscores",
            });
        }
    }
    Ok(())
}

/// Validate that the predicate template carries the required
/// `LIMIT $1` placeholder AND no other `$n` placeholders. The contract
/// is documented at module level: the pattern emitter owns the SQL
/// fragment, but the runner pins the shape so a missing `LIMIT` (which
/// would unbound the chunk) or a stray placeholder (which the runner
/// would not bind) is caught up front rather than producing a runtime
/// "unbound parameter" error mid-run.
///
/// The check is a byte-level scan over the template (no regex per
/// CLAUDE.md). The walker treats `$<digit>` as a placeholder token:
///
/// - `$1` is the only accepted placeholder. Anything else (`$2`, `$3`,
///   `$10`, …) is an unbound parameter and rejected.
/// - `$<non-digit>` is left alone. Postgres dollar-quoted string
///   literals (`$tag$ … $tag$`) and other non-placeholder uses of `$`
///   live in this branch; the runner does not try to parse Postgres
///   string-literal syntax beyond this point.
///
/// The `LIMIT $1` substring must appear at least once.
fn validate_predicate_template(template: &str) -> Result<(), BackfillError> {
    if !template.contains("LIMIT $1") {
        return Err(BackfillError::MalformedPredicateTemplate);
    }
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        // Found `$`. Look at the next byte.
        let next = match bytes.get(i + 1) {
            Some(&b) => b,
            None => break, // trailing `$` with no follow-up byte — not a placeholder.
        };
        if !next.is_ascii_digit() {
            // `$<non-digit>` — not a placeholder (could be a dollar-quoted
            // string tag or an unrelated literal `$`). Skip past the `$`.
            i += 1;
            continue;
        }
        // `$<digit>`. Walk every consecutive digit so multi-digit
        // placeholders (`$10`, `$11`) are recognised as a single token.
        let digits_start = i + 1;
        let mut digits_end = digits_start;
        while digits_end < bytes.len() && bytes[digits_end].is_ascii_digit() {
            digits_end += 1;
        }
        // Accept exactly the single ASCII byte `1` — any longer digit
        // run (e.g. `$10`) or any other single digit (`$2`) is a
        // placeholder the runner does not bind.
        let digit_run = digits_end - digits_start;
        if !(digit_run == 1 && bytes[digits_start] == b'1') {
            return Err(BackfillError::MalformedPredicateTemplate);
        }
        i = digits_end;
    }
    Ok(())
}

/// Compute the cumulative `rows_done_total` after a chunk of
/// `rows_just_added` lands on a row that already had `rows_already`
/// done. Saturating to `i64::MAX as u64` so the in-memory value can
/// never exceed what the persisted `djogi_live_plans.backfill_rows_done`
/// column (`BIGINT`, max `i64::MAX`) is able to store. In practice no
/// real-world backfill approaches `2^63` rows, but defensive saturation
/// at the persisted boundary costs nothing and keeps the in-memory and
/// on-disk values aligned.
///
/// Lifted into a free function purely so the unit tests can pin its
/// edge cases without spinning up a `DjogiContext`.
pub(crate) fn compute_rows_done_total(rows_already: u64, rows_just_added: u64) -> u64 {
    const PERSISTED_MAX: u64 = i64::MAX as u64;
    let raw = rows_already.saturating_add(rows_just_added);
    if raw > PERSISTED_MAX {
        PERSISTED_MAX
    } else {
        raw
    }
}

// ── execute / resume ──────────────────────────────────────────────────

/// Execute a [`StepKind::BackfillChunked`](crate::live_migrate::plan::StepKind::BackfillChunked)
/// step to completion via repeated chunks. Starts at
/// `backfill_rows_done = 0` (initial run) — see [`resume_backfill`] for
/// the resume entry point.
///
/// Each chunk:
///
/// 1. Opens a fresh transaction on the application connection (via
///    [`atomic`]).
/// 2. `SET LOCAL "djogi.live_migrate.suppress_events" = '1'` unless
///    `emit_events` is `true`.
/// 3. Runs `UPDATE <table> <predicate_template>` with `chunk_size`
///    bound to `$1`.
/// 4. Counts the affected rows.
/// 5. Updates `djogi_live_plans.backfill_rows_done` and
///    `last_progress_at` in the SAME transaction.
/// 6. Commits.
///
/// Loops while `rows_affected == chunk_size`. When `rows_affected <
/// chunk_size` the predicate has exhausted and the runner transitions
/// the plan to [`PlanStatus::Validating`] before returning.
///
/// # Preconditions
///
/// - `ctx` must be pool-backed (i.e. not already inside a transaction).
///   Each chunk opens its own transaction; nesting inside an outer
///   transaction would defeat the per-chunk commit boundary.
/// - The plan referenced by `plan_id` must exist in
///   `djogi_live_plans` and be in [`PlanStatus::Running`]. The
///   operator's CLI promotes the plan into `Running` via `djogi live
///   run`; the runner does not auto-promote.
pub async fn execute_backfill(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
    table: &str,
    predicate_template: &str,
    chunk_size: u32,
    emit_events: bool,
) -> Result<Vec<BackfillChunk>, BackfillError> {
    drive_chunks(
        ctx,
        plan_id,
        table,
        predicate_template,
        chunk_size,
        emit_events,
        ResumeFrom::Initial,
    )
    .await
}

/// Resume a backfill from where the last execution stopped. Reads
/// `backfill_rows_done` from `djogi_live_plans`, then drives chunks
/// forward until the predicate exhausts. The per-chunk transaction
/// shape is identical to [`execute_backfill`].
///
/// Resume is idempotent over the predicate contract: if the previous
/// run committed N rows and crashed before writing `N + 1`, this
/// function reads `backfill_rows_done = N` and continues; if the
/// previous run wrote both the chunk and the progress row in the same
/// transaction (the contract this module enforces), the resume picks
/// up exactly where it left off. The pattern's `WHERE` clause being
/// idempotent guarantees that even a "double-replay" of the last chunk
/// would be a no-op.
pub async fn resume_backfill(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
    table: &str,
    predicate_template: &str,
    chunk_size: u32,
    emit_events: bool,
) -> Result<Vec<BackfillChunk>, BackfillError> {
    drive_chunks(
        ctx,
        plan_id,
        table,
        predicate_template,
        chunk_size,
        emit_events,
        ResumeFrom::Persisted,
    )
    .await
}

/// Whether [`drive_chunks`] should treat the run as a fresh execution
/// (starting from `rows_done = 0`) or a resume (reading the persisted
/// `backfill_rows_done` from `djogi_live_plans`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeFrom {
    Initial,
    Persisted,
}

/// Shared chunk-loop body for [`execute_backfill`] and
/// [`resume_backfill`]. Validates inputs, locates the plan row, and
/// loops chunks until the predicate exhausts.
#[allow(clippy::too_many_arguments)]
async fn drive_chunks(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
    table: &str,
    predicate_template: &str,
    chunk_size: u32,
    emit_events: bool,
    resume_from: ResumeFrom,
) -> Result<Vec<BackfillChunk>, BackfillError> {
    if chunk_size == 0 {
        return Err(BackfillError::ZeroChunkSize);
    }
    validate_table_ident(table)?;
    validate_predicate_template(predicate_template)?;

    let pool = ctx
        .raw_pool()
        .ok_or_else(|| {
            DjogiError::Db(DbError::other(
                "execute_backfill / resume_backfill require a pool-backed DjogiContext; \
                 each chunk opens its own transaction and cannot nest inside an outer one",
            ))
        })?
        .clone();

    // Locate the plan row and verify state. We need the bucket key
    // (target_database, app_label) to write progress; plan_id alone is
    // unique (it is the table's PRIMARY KEY) but the existing helpers
    // take the bucket fields to keep the §6.5 multi-app boundary
    // explicit at every call site.
    let row = lookup_plan_row(ctx, plan_id).await?;
    if row.status != PlanStatus::Running {
        return Err(BackfillError::NotRunning(plan_id, row.status.as_db_str()));
    }
    let starting_rows_done = match resume_from {
        ResumeFrom::Initial => 0_u64,
        ResumeFrom::Persisted => row.backfill_rows_done.max(0) as u64,
    };
    let target_database = row.target_database.clone();
    let app_label = row.app_label.clone();

    // Build the per-chunk SQL once. `table` was validated above, so the
    // concatenation is safe. The pattern's template carries the `SET …`
    // clause through `RETURNING …`.
    let chunk_sql = format!("UPDATE {table} {predicate_template}");
    let chunk_size_i64 = i64::from(chunk_size);

    let mut chunks: Vec<BackfillChunk> = Vec::new();
    let mut rows_done_total = starting_rows_done;
    let mut ordinal: u64 = 0;
    let chunk_size_u64 = u64::from(chunk_size);

    loop {
        ordinal = ordinal.saturating_add(1);
        let outcome = run_one_chunk(
            &pool,
            &chunk_sql,
            chunk_size_i64,
            chunk_size_u64,
            emit_events,
            plan_id,
            &target_database,
            &app_label,
            rows_done_total,
        )
        .await?;
        rows_done_total = compute_rows_done_total(rows_done_total, outcome.rows_affected);
        chunks.push(BackfillChunk {
            ordinal,
            rows_affected: outcome.rows_affected,
            rows_done_total,
        });

        if outcome.rows_affected == 0 && ordinal == 1 {
            // First chunk reports zero rows. Two legitimate sub-cases:
            //
            // 1. Initial run, predicate matched nothing on entry — the
            //    table was empty or already backfilled. Treat as
            //    immediate exhaustion. The `run_one_chunk` closure
            //    already promoted the plan to `Validating` inside the
            //    same transaction (it observed `rows_affected <
            //    chunk_size`), so the on-disk row is consistent with
            //    the in-memory `chunks` we are about to return.
            //
            // 2. Resume after crash: a prior run committed the last
            //    chunk's UPDATE + progress row but crashed before the
            //    status promotion landed. On resume, the predicate
            //    yields 0 rows on the FIRST chunk (because the prior
            //    run already drained the table) but `starting_rows_done
            //    > 0` records the progress that survived. The runner
            //    must NOT fire `PredicateStuck` in this case — the
            //    closure idempotently re-promotes status to
            //    `Validating` and we exit cleanly. Without this branch
            //    a crash between the last chunk's commit and the
            //    historical out-of-transaction status update would
            //    leave the plan permanently stuck on resume.
            break;
        }
        if outcome.rows_affected == 0 {
            // Subsequent chunk reported zero work but the predicate did
            // not exhaust on the previous chunk's count. Either the
            // table is empty (legitimate exhaustion — caught above for
            // the first-chunk case) or the predicate is no longer
            // matching rows it was matching a moment ago — fail loud
            // and let the operator investigate.
            return Err(BackfillError::PredicateStuck);
        }
        if outcome.rows_affected < chunk_size_u64 {
            // Partial chunk — predicate exhausted. The closure already
            // promoted status to `Validating` inside the same atomic
            // block; nothing more to do here.
            break;
        }
        // Otherwise full chunk; loop continues.
    }

    Ok(chunks)
}

/// One chunk's worth of work, all inside a single `atomic()` scope so
/// the chunk's `UPDATE`, the progress write, AND (when this chunk
/// exhausts the predicate) the `Running → Validating` status
/// promotion commit together. Pinning all three writes to the same
/// transaction makes the resume contract atomic by construction: a
/// crash between the chunk's commit and the status promotion is
/// impossible because the two writes share a transaction boundary.
#[allow(clippy::too_many_arguments)]
async fn run_one_chunk(
    pool: &crate::pg::pool::DjogiPool,
    chunk_sql: &str,
    chunk_size_i64: i64,
    chunk_size_u64: u64,
    emit_events: bool,
    plan_id: HeerId,
    target_database: &str,
    app_label: &str,
    prior_rows_done: u64,
) -> Result<ChunkOutcome, BackfillError> {
    let chunk_sql_owned = chunk_sql.to_owned();
    let target_database_owned = target_database.to_owned();
    let app_label_owned = app_label.to_owned();

    let outcome = atomic(pool, move |tx_ctx| {
        Box::pin(async move {
            if !emit_events {
                // `set_config(name, value, true)` is the function form
                // of `SET LOCAL` — the GUC is scoped to the current
                // transaction. The GUC name is the const literal
                // [`SIDE_EFFECT_SUPPRESSION_TXN_LOCAL`] so the SQL
                // text contains no user-supplied path; the value is
                // bound via `$1` rather than interpolated.
                tx_ctx
                    .execute(SUPPRESS_EVENTS_SET_LOCAL_SQL.as_str(), &[&"1"])
                    .await?;
            }

            let rows_affected = tx_ctx.execute(&chunk_sql_owned, &[&chunk_size_i64]).await?;

            let new_rows_done_u64 = compute_rows_done_total(prior_rows_done, rows_affected);
            // `compute_rows_done_total` already saturates at
            // `i64::MAX as u64`, so the cast cannot lose precision —
            // BIGINT range is the in-memory ceiling by construction.
            let new_rows_done_i64 = i64::try_from(new_rows_done_u64).unwrap_or(i64::MAX);
            state::update_progress(
                tx_ctx,
                plan_id,
                &target_database_owned,
                &app_label_owned,
                new_rows_done_i64,
            )
            .await?;

            // Predicate-exhaustion check: if the chunk drained fewer
            // rows than the chunk size, the next loop iteration would
            // observe zero rows and break. Promote status to
            // `Validating` HERE — inside the same transaction — so the
            // operator's resume path always sees a self-consistent
            // (rows_done, status) pair on disk.
            if rows_affected < chunk_size_u64 {
                state::update_status(
                    tx_ctx,
                    plan_id,
                    &target_database_owned,
                    &app_label_owned,
                    PlanStatus::Validating,
                )
                .await?;
            }

            Ok::<ChunkOutcome, DjogiError>(ChunkOutcome { rows_affected })
        })
    })
    .await
    .map_err(BackfillError::from)?;

    Ok(outcome)
}

/// Internal carrier for one chunk's reported work — kept distinct from
/// the public [`BackfillChunk`] so the chunk-loop can compute the
/// cumulative `rows_done_total` outside the transaction closure.
struct ChunkOutcome {
    rows_affected: u64,
}

/// Look up the plan row by its globally-unique `plan_id`. Used by the
/// chunk runner before opening any transaction, so callers can fail
/// fast on a missing or wrong-state plan without paying for a chunk
/// transaction's BEGIN/ROLLBACK.
async fn lookup_plan_row(
    ctx: &mut DjogiContext,
    plan_id: HeerId,
) -> Result<state::LivePlanRow, BackfillError> {
    // Direct SELECT by primary key — bypasses the bucket-keyed
    // `fetch_row_by_id` because at this entry point the caller has only
    // the plan_id (the bucket is what we need to read out of the row,
    // not assert against).
    let sql = "SELECT plan_id, slug, plan_file_checksum, classification, status, \
               current_step, current_step_index, backfill_rows_done, \
               backfill_rows_total, started_at, last_progress_at, completed_at, \
               last_error, originating_migration, target_database, app_label \
               FROM djogi_live_plans WHERE plan_id = $1";
    let plan_id_i64 = plan_id.as_i64();
    let row_opt = ctx
        .query_opt(sql, &[&plan_id_i64])
        .await
        .map_err(BackfillError::from)?;
    let row = row_opt.ok_or(BackfillError::PlanNotFound(plan_id))?;
    parse_live_plan_row(&row).map_err(BackfillError::from)
}

/// Mirror of the column-order-dependent parser in `state.rs`. Kept
/// here as a private helper because `state.rs`'s `row_to_live_plan_row`
/// is module-private; rather than widen its visibility, this module
/// owns its own parser anchored to the same SELECT shape.
fn parse_live_plan_row(row: &tokio_postgres::Row) -> Result<state::LivePlanRow, DjogiError> {
    use crate::live_migrate::plan::PlanClassification;
    use time::OffsetDateTime;

    let plan_id_i64: i64 = row
        .try_get(0)
        .map_err(|e| DjogiError::Db(DbError::other(format!("plan_id read failed: {e}"))))?;
    let plan_id = HeerId::from_i64(plan_id_i64)
        .map_err(|e| DjogiError::Db(DbError::other(format!("invalid plan_id in row: {e}"))))?;
    let slug: String = row
        .try_get(1)
        .map_err(|e| DjogiError::Db(DbError::other(format!("slug read failed: {e}"))))?;
    let plan_file_checksum: String = row
        .try_get(2)
        .map_err(|e| DjogiError::Db(DbError::other(format!("checksum read failed: {e}"))))?;
    let classification_s: String = row
        .try_get(3)
        .map_err(|e| DjogiError::Db(DbError::other(format!("classification read failed: {e}"))))?;
    let classification = PlanClassification::from_db_str(&classification_s).ok_or_else(|| {
        DjogiError::Db(DbError::other(format!(
            "unknown classification in djogi_live_plans: {classification_s:?}"
        )))
    })?;
    let status_s: String = row
        .try_get(4)
        .map_err(|e| DjogiError::Db(DbError::other(format!("status read failed: {e}"))))?;
    let status = PlanStatus::from_db_str(&status_s).ok_or_else(|| {
        DjogiError::Db(DbError::other(format!(
            "unknown status in djogi_live_plans: {status_s:?}"
        )))
    })?;
    let current_step: Option<String> = row
        .try_get(5)
        .map_err(|e| DjogiError::Db(DbError::other(format!("current_step read failed: {e}"))))?;
    let current_step_index: i32 = row.try_get(6).map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "current_step_index read failed: {e}"
        )))
    })?;
    let backfill_rows_done: i64 = row.try_get(7).map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "backfill_rows_done read failed: {e}"
        )))
    })?;
    let backfill_rows_total: Option<i64> = row.try_get(8).map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "backfill_rows_total read failed: {e}"
        )))
    })?;
    let started_at: Option<OffsetDateTime> = row
        .try_get(9)
        .map_err(|e| DjogiError::Db(DbError::other(format!("started_at read failed: {e}"))))?;
    let last_progress_at: Option<OffsetDateTime> = row.try_get(10).map_err(|e| {
        DjogiError::Db(DbError::other(format!("last_progress_at read failed: {e}")))
    })?;
    let completed_at: Option<OffsetDateTime> = row
        .try_get(11)
        .map_err(|e| DjogiError::Db(DbError::other(format!("completed_at read failed: {e}"))))?;
    let last_error: Option<String> = row
        .try_get(12)
        .map_err(|e| DjogiError::Db(DbError::other(format!("last_error read failed: {e}"))))?;
    let originating_migration: String = row.try_get(13).map_err(|e| {
        DjogiError::Db(DbError::other(format!(
            "originating_migration read failed: {e}"
        )))
    })?;
    let target_database: String = row
        .try_get(14)
        .map_err(|e| DjogiError::Db(DbError::other(format!("target_database read failed: {e}"))))?;
    let app_label: String = row
        .try_get(15)
        .map_err(|e| DjogiError::Db(DbError::other(format!("app_label read failed: {e}"))))?;
    Ok(state::LivePlanRow {
        plan_id,
        slug,
        plan_file_checksum,
        classification,
        status,
        current_step,
        current_step_index,
        backfill_rows_done,
        backfill_rows_total,
        started_at,
        last_progress_at,
        completed_at,
        last_error,
        originating_migration,
        target_database,
        app_label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── BackfillError display ────────────────────────────────────────

    #[test]
    fn backfill_error_plan_not_found_displays_plan_id() {
        let id = HeerId::from_i64(42).expect("HeerId::from_i64(42) is valid");
        let msg = format!("{}", BackfillError::PlanNotFound(id));
        assert!(msg.contains("42"), "expected plan id in message: {msg}");
        assert!(msg.contains("not found"), "expected `not found`: {msg}");
    }

    #[test]
    fn backfill_error_not_running_displays_status_label() {
        let id = HeerId::from_i64(7).expect("HeerId::from_i64(7) is valid");
        let msg = format!("{}", BackfillError::NotRunning(id, "paused"));
        assert!(msg.contains("7"), "expected plan id: {msg}");
        assert!(msg.contains("paused"), "expected status label: {msg}");
    }

    #[test]
    fn backfill_error_non_idempotent_predicate_message_actionable() {
        let msg = format!("{}", BackfillError::NonIdempotentPredicate);
        assert!(msg.contains("idempotent"), "expected `idempotent`: {msg}");
    }

    #[test]
    fn backfill_error_malformed_template_mentions_limit() {
        let msg = format!("{}", BackfillError::MalformedPredicateTemplate);
        assert!(msg.contains("LIMIT"), "expected `LIMIT` hint: {msg}");
        assert!(msg.contains("$1"), "expected `$1` hint: {msg}");
    }

    #[test]
    fn backfill_error_invalid_table_includes_table_and_reason() {
        let err = BackfillError::InvalidTable {
            table: "1bad".to_owned(),
            reason: "table name must start with an ASCII letter or underscore",
        };
        let msg = format!("{err}");
        assert!(msg.contains("1bad"), "expected table name: {msg}");
        assert!(
            msg.contains("ASCII letter or underscore"),
            "expected reason: {msg}",
        );
    }

    #[test]
    fn backfill_error_zero_chunk_size_actionable() {
        let msg = format!("{}", BackfillError::ZeroChunkSize);
        assert!(
            msg.contains("greater than zero") || msg.contains("> 0"),
            "expected actionable hint: {msg}",
        );
    }

    #[test]
    fn backfill_error_predicate_stuck_actionable() {
        let msg = format!("{}", BackfillError::PredicateStuck);
        assert!(
            msg.contains("0 rows") && msg.contains("exhaust"),
            "expected stuck-predicate hint: {msg}",
        );
    }

    // ── compute_rows_done_total ──────────────────────────────────────

    #[test]
    fn compute_rows_done_total_zero_added_is_passthrough() {
        assert_eq!(compute_rows_done_total(0, 0), 0);
        assert_eq!(compute_rows_done_total(7, 0), 7);
    }

    #[test]
    fn compute_rows_done_total_typical_chunk() {
        // First chunk: starting at 0, 10000 rows added.
        assert_eq!(compute_rows_done_total(0, 10_000), 10_000);
        // Second chunk: cumulative grows by another 10000.
        assert_eq!(compute_rows_done_total(10_000, 10_000), 20_000);
        // Final partial chunk: cumulative grows by less than chunk_size.
        assert_eq!(compute_rows_done_total(20_000, 4_321), 24_321);
    }

    #[test]
    fn compute_rows_done_total_saturates_on_overflow() {
        // Defensive — no real backfill approaches 2^63, but saturating
        // at the persisted-column boundary (`BIGINT`, max `i64::MAX`)
        // keeps the in-memory value aligned with what
        // `djogi_live_plans.backfill_rows_done` can actually store.
        let cap = i64::MAX as u64;
        assert_eq!(
            compute_rows_done_total(cap - 5, 100),
            cap,
            "overflow must saturate at i64::MAX as u64, not u64::MAX",
        );
        assert_eq!(compute_rows_done_total(cap, 1), cap);
        // Exactly at the cap is a passthrough.
        assert_eq!(compute_rows_done_total(cap, 0), cap);
        // Any value `u64::MAX - n` for small n must NOT survive — it
        // exceeds i64::MAX and must clamp down to the persisted ceiling.
        assert_eq!(compute_rows_done_total(u64::MAX - 5, 100), cap);
        assert_eq!(compute_rows_done_total(u64::MAX, 0), cap);
    }

    // ── validate_table_ident ─────────────────────────────────────────

    #[test]
    fn validate_table_ident_accepts_plain_identifiers() {
        assert!(validate_table_ident("vehicles").is_ok());
        assert!(validate_table_ident("vehicle_audit_log").is_ok());
        assert!(validate_table_ident("_internal").is_ok());
        assert!(validate_table_ident("t1").is_ok());
        // 63 bytes — Postgres's NAMEDATALEN - 1 limit.
        let at_limit: String = "a".repeat(63);
        assert!(validate_table_ident(&at_limit).is_ok());
    }

    #[test]
    fn validate_table_ident_rejects_empty() {
        let err = validate_table_ident("").expect_err("empty must fail");
        match err {
            BackfillError::InvalidTable { table, .. } => assert_eq!(table, ""),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn validate_table_ident_rejects_leading_digit() {
        let err = validate_table_ident("9vehicles").expect_err("leading digit must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("ASCII letter or underscore"),
            "expected leading-byte reason: {msg}",
        );
    }

    #[test]
    fn validate_table_ident_rejects_punctuation_payload() {
        // Classic SQL-injection shape — the runner pastes the table
        // into SQL directly, so accepting punctuation would be a
        // vulnerability.
        assert!(validate_table_ident("vehicles; DROP TABLE users;").is_err());
        assert!(validate_table_ident("vehicles\"; --").is_err());
        assert!(validate_table_ident("public.vehicles").is_err()); // dot not allowed
    }

    #[test]
    fn validate_table_ident_rejects_oversized() {
        // 64 bytes — one over the limit.
        let oversized: String = "a".repeat(64);
        let err = validate_table_ident(&oversized).expect_err("oversized must fail");
        let msg = format!("{err}");
        assert!(msg.contains("63"), "expected 63-byte limit reason: {msg}");
    }

    #[test]
    fn validate_table_ident_rejects_non_ascii() {
        // Multi-byte UTF-8 char — every byte fails the alphanumeric
        // check on the per-byte loop.
        assert!(validate_table_ident("café").is_err());
        assert!(validate_table_ident("naïve_table").is_err());
    }

    // ── validate_predicate_template ──────────────────────────────────

    #[test]
    fn validate_predicate_template_accepts_canonical_shape() {
        let template =
            "SET new_col = derive_from_old(old_col) WHERE new_col IS NULL LIMIT $1 RETURNING id";
        assert!(validate_predicate_template(template).is_ok());
    }

    #[test]
    fn validate_predicate_template_rejects_missing_limit() {
        let template = "SET new_col = derive(old_col) WHERE new_col IS NULL RETURNING id";
        let err = validate_predicate_template(template).expect_err("missing LIMIT $1 must fail");
        assert!(matches!(err, BackfillError::MalformedPredicateTemplate));
    }

    #[test]
    fn validate_predicate_template_rejects_wrong_placeholder() {
        // `LIMIT $2` — wrong placeholder index. Caught because the
        // template lacks `LIMIT $1`.
        let template = "SET new_col = derive(old_col) WHERE new_col IS NULL LIMIT $2 RETURNING id";
        let err = validate_predicate_template(template).expect_err("wrong placeholder must fail");
        assert!(matches!(err, BackfillError::MalformedPredicateTemplate));
    }

    #[test]
    fn validate_predicate_template_rejects_extra_placeholder_alongside_limit() {
        // Template carries the required `LIMIT $1` AND a stray `$2`
        // the runner would never bind. Must reject — this is the bug
        // the byte-scan walker exists to catch.
        let template = "SET new_col = $2 WHERE new_col IS NULL LIMIT $1 RETURNING id";
        let err = validate_predicate_template(template).expect_err("stray $2 must fail");
        assert!(matches!(err, BackfillError::MalformedPredicateTemplate));
    }

    #[test]
    fn validate_predicate_template_rejects_three_or_higher() {
        // Same shape as above with `$3` and `$9` — every placeholder
        // other than `$1` must be rejected.
        for stray in ["$3", "$4", "$5", "$6", "$7", "$8", "$9"] {
            let template = format!(
                "SET new_col = derive({stray}, old_col) WHERE new_col IS NULL LIMIT $1 RETURNING id",
            );
            let err = validate_predicate_template(&template)
                .err()
                .unwrap_or_else(|| panic!("expected reject for stray {stray}; got ok"));
            assert!(matches!(err, BackfillError::MalformedPredicateTemplate));
        }
    }

    #[test]
    fn validate_predicate_template_rejects_multi_digit_placeholder() {
        // `$10` is a valid Postgres placeholder but the runner only
        // binds `$1` — a multi-digit run must reject even though the
        // first digit happens to be `1`.
        let template = "SET new_col = $10 WHERE new_col IS NULL LIMIT $1 RETURNING id";
        let err = validate_predicate_template(template).expect_err("$10 must fail");
        assert!(matches!(err, BackfillError::MalformedPredicateTemplate));
        // Same shape with `$11`.
        let template = "SET new_col = $11 WHERE new_col IS NULL LIMIT $1 RETURNING id";
        let err = validate_predicate_template(template).expect_err("$11 must fail");
        assert!(matches!(err, BackfillError::MalformedPredicateTemplate));
    }

    #[test]
    fn validate_predicate_template_allows_dollar_followed_by_non_digit() {
        // Postgres dollar-quoted string literals (`$tag$ … $tag$`) and
        // other non-placeholder `$` uses must NOT be flagged. The
        // walker only treats `$<digit>` as a placeholder; `$<letter>`
        // is left alone.
        let template = "SET new_col = $tag$value$tag$ WHERE new_col IS NULL LIMIT $1 RETURNING id";
        assert!(validate_predicate_template(template).is_ok());
        let template = "SET new_col = '$' WHERE new_col IS NULL LIMIT $1 RETURNING id";
        assert!(validate_predicate_template(template).is_ok());
    }

    #[test]
    fn validate_predicate_template_handles_trailing_dollar() {
        // Trailing `$` with no follow-up byte — not a placeholder.
        let template = "SET col = 'price$' WHERE col IS NULL LIMIT $1";
        assert!(validate_predicate_template(template).is_ok());
    }

    // ── side-effect suppression GUC name pinning ─────────────────────

    #[test]
    fn side_effect_suppression_guc_name_uses_djogi_namespace() {
        // Postgres requires custom GUCs to live under a dotted
        // namespace; pinning the "djogi.*" prefix here so a future
        // refactor can't silently move the flag out of the framework's
        // namespace.
        assert!(
            SIDE_EFFECT_SUPPRESSION_TXN_LOCAL.starts_with("djogi."),
            "side-effect suppression flag must live under djogi.*: {SIDE_EFFECT_SUPPRESSION_TXN_LOCAL:?}",
        );
        assert!(
            SIDE_EFFECT_SUPPRESSION_TXN_LOCAL.contains("suppress_events"),
            "name must reflect intent: {SIDE_EFFECT_SUPPRESSION_TXN_LOCAL:?}",
        );
    }

    #[test]
    fn suppress_events_set_local_sql_embeds_canonical_guc_name() {
        // Drift guard: the SQL is stitched together off the canonical
        // GUC name via `format!`. Renaming the GUC must thread through
        // automatically; this assertion fails-fast if the lazy
        // initializer ever drifts off the canonical const.
        let sql = SUPPRESS_EVENTS_SET_LOCAL_SQL.as_str();
        assert!(
            sql.contains(SIDE_EFFECT_SUPPRESSION_TXN_LOCAL),
            "set_config SQL must embed the canonical GUC name verbatim: \
             SQL = {sql:?}, GUC = {SIDE_EFFECT_SUPPRESSION_TXN_LOCAL:?}",
        );
        // `set_config(name, value, is_local)` is the canonical Postgres
        // function-form of `SET LOCAL`; the third argument `true`
        // anchors the transaction-scoped semantics.
        assert!(
            sql.contains("set_config") && sql.contains("$1") && sql.contains("true"),
            "set_config SQL must use the function form with `true` (SET LOCAL): {sql:?}",
        );
    }
}
