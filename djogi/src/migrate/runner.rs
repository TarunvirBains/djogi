//! Migration runner — applies a [`MigrationPlan`] against a target
//! database, recording each step in the `djogi_schema_migrations`
//! ledger and persisting the snapshot only on full success.
//!
//! # Lifecycle (Phase 7 v3 §6 / §8)
//!
//! ```text
//! 1. Acquire workspace file lock (T4 guard primitive).
//! 2. Bootstrap djogi_schema_migrations table.
//! 3. Acquire pg_advisory_lock on a 64-bit key derived from BucketKey.
//! 4. Verify the supplied checksum matches a freshly-computed one.
//! 5. Insert the pending ledger row OUTSIDE the apply transaction.
//! 6. For each segment, dispatch by SegmentKind:
//!      - Transactional   → BEGIN; statements; COMMIT.
//!      - NonTransactional → autocommit each statement; update progress.
//!      - MetadataOnly    → no SQL runs; metadata path is T6's job.
//! 7. On success: mark_applied + persist snapshot.
//! 8. On failure: mark_failed (or mark_partial for split-apply)
//!    and propagate. Snapshot is NOT moved forward.
//! 9. Always: release pg_advisory_lock and workspace file lock.
//! ```
//!
//! # Snapshot persistence invariant
//!
//! The snapshot file at `migrations/<target>/<app>/schema_snapshot.json`
//! is written ONLY after the ledger row reaches `applied`. Any failure
//! — transactional rollback, non-transactional crash, ledger update
//! error — leaves the snapshot at its prior value. This is the hard
//! invariant T4 owes T5 (`repair`) and T7 (`status`): if the snapshot
//! moved, every preceding migration succeeded.
//!
//! # Determinism
//!
//! Two runs against the same plan + same DB state must produce the
//! same ledger writes (modulo `applied_at`, `applied_by`,
//! `execution_time_ms`, and `run_id`). Hashing of `BucketKey` to the
//! advisory-lock key uses SHA-256 truncated to the low 64 bits — a
//! stable hash that does not depend on Rust's randomised default
//! `Hasher`.
//!
//! # Relpages probe (Phase 7-Zero v3 §6.5)
//!
//! Before every transactional `CREATE INDEX` whose `IndexSchema`
//! does NOT carry `requires_out_of_transaction == true`, the runner
//! queries `pg_class.relpages` for the target table. When relpages
//! exceeds [`MigrateConfig::concurrent_warn_relpages`] (default 128
//! pages ≈ 1 MB), the runner emits `tracing::warn!` advising the
//! operator to opt the index into `CREATE INDEX CONCURRENTLY`. With
//! [`MigrateConfig::strict_concurrent_warnings`] the warn upgrades
//! to a hard `RunnerError::RelpagesThresholdExceeded`.

use std::path::PathBuf;
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::config::MigrateConfig;
use crate::context::DjogiContext;
use crate::error::DjogiError;
use crate::types::HeerId;

use super::ledger::{
    self, ChecksumMismatch, ExecutionMode, LedgerRow, LedgerStatus, compute_checksum,
};
use super::projection::BucketKey;
use super::schema::SNAPSHOT_FORMAT_VERSION;
use super::segment::{MigrationPlan, Segment, SegmentKind};
use super::snapshot::{SnapshotError, save_snapshot};
use super::sql::OperationSql;

// ── Public types ──────────────────────────────────────────────────────────

/// Errors surfaced by the runner. Each variant carries enough context
/// for an actionable operator message — no panicking, no silent
/// drops.
#[derive(Debug)]
pub enum RunnerError {
    /// Workspace file lock could not be acquired within the timeout.
    LockTimeout {
        path: PathBuf,
        holder_pid: Option<i32>,
    },

    /// File-level workspace lock errored for any reason other than
    /// timeout (I/O, kernel error, Windows-not-supported).
    GuardError(super::guard::GuardError),

    /// Postgres advisory lock could not be acquired within the
    /// retry budget. Distinct from `LockTimeout` (file-lock) so the
    /// operator can disambiguate the two layers.
    AdvisoryLockFailed {
        bucket: BucketKey,
        key: i64,
        attempts: u32,
    },

    /// Stored checksum does not match a freshly-computed one. The
    /// runner refuses to apply when a migration's SQL has been
    /// edited after it was committed.
    ChecksumMismatch(ChecksumMismatch),

    /// A ledger row CRUD operation failed (INSERT, UPDATE).
    LedgerWriteFailed { version: String, source: DjogiError },

    /// A `CREATE INDEX` was about to run against a table whose
    /// `pg_class.relpages` exceeded the operator-configured
    /// threshold, AND `migrate.strict_concurrent_warnings` is true.
    /// Surface fields match the warn-path emit so logs and errors
    /// share identifiers.
    RelpagesThresholdExceeded {
        bucket: BucketKey,
        index_name: String,
        target_table: String,
        relpages: i32,
        threshold: u32,
    },

    /// A statement inside a transactional segment failed; the
    /// transaction was rolled back. Carries the failing statement
    /// label and the underlying error.
    TransactionalSegmentFailed {
        segment_index: usize,
        statement_label: String,
        source: DjogiError,
    },

    /// A statement inside a non-transactional segment failed. The
    /// runner records `applied_steps_count` so a `repair` invocation
    /// can resume from the next step.
    NonTransactionalSegmentFailed {
        segment_index: usize,
        step_index: usize,
        statement_label: String,
        applied_steps_count: i32,
        source: DjogiError,
    },

    /// Failed to load `Djogi.toml` for the relpages-probe config.
    ConfigLoadFailed { source: figment::Error },

    /// Snapshot file write failed AFTER the ledger row was marked
    /// applied. The runner surfaces this as a separate variant
    /// because the operator's recovery path is "manually invoke
    /// `compose` to regenerate the snapshot" — different from a
    /// rollback path.
    SnapshotPersistFailed {
        path: PathBuf,
        source: SnapshotError,
    },
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunnerError::LockTimeout { path, holder_pid } => match holder_pid {
                Some(pid) => write!(
                    f,
                    "D025 lock held by another invocation (PID {pid}) at {}; \
                     refusing to apply",
                    path.display(),
                ),
                None => write!(
                    f,
                    "D025 lock held by another invocation at {}; refusing to apply \
                     (PID unknown)",
                    path.display(),
                ),
            },
            RunnerError::GuardError(e) => write!(f, "workspace lock error: {e}"),
            RunnerError::AdvisoryLockFailed {
                bucket,
                key,
                attempts,
            } => write!(
                f,
                "Postgres advisory lock for bucket database={db} app={app} \
                 (key=0x{key:016x}) could not be acquired after {attempts} attempts",
                db = bucket.database,
                app = bucket.app,
            ),
            RunnerError::ChecksumMismatch(m) => write!(f, "{m}"),
            RunnerError::LedgerWriteFailed { version, source } => {
                write!(f, "ledger write failed for version `{version}`: {source}")
            }
            RunnerError::RelpagesThresholdExceeded {
                bucket,
                index_name,
                target_table,
                relpages,
                threshold,
            } => write!(
                f,
                "relpages probe rejected `CREATE INDEX {index}` on `{table}` (bucket \
                 database={db} app={app}): {relpages} > {threshold}; \
                 set `requires_out_of_transaction = true` on the IndexSpec, or \
                 lower `migrate.strict_concurrent_warnings`",
                index = index_name,
                table = target_table,
                db = bucket.database,
                app = bucket.app,
            ),
            RunnerError::TransactionalSegmentFailed {
                segment_index,
                statement_label,
                source,
            } => write!(
                f,
                "transactional segment {segment_index} failed at `{statement_label}`: {source}",
            ),
            RunnerError::NonTransactionalSegmentFailed {
                segment_index,
                step_index,
                statement_label,
                applied_steps_count,
                source,
            } => write!(
                f,
                "non-transactional segment {segment_index} step {step_index} `{statement_label}` \
                 failed after {applied_steps_count} successful step(s): {source}",
            ),
            RunnerError::ConfigLoadFailed { source } => {
                write!(f, "failed to load Djogi.toml: {source}")
            }
            RunnerError::SnapshotPersistFailed { path, source } => {
                write!(f, "snapshot persist failed at {}: {source}", path.display(),)
            }
        }
    }
}

impl std::error::Error for RunnerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunnerError::ChecksumMismatch(e) => Some(e),
            RunnerError::GuardError(e) => Some(e),
            RunnerError::LedgerWriteFailed { source, .. } => Some(source),
            RunnerError::TransactionalSegmentFailed { source, .. } => Some(source),
            RunnerError::NonTransactionalSegmentFailed { source, .. } => Some(source),
            RunnerError::ConfigLoadFailed { source } => Some(source),
            RunnerError::SnapshotPersistFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Caller-supplied context for one runner invocation. Decouples the
/// runner from a hard-coded `Djogi.toml` location so tests can
/// inject their own knobs.
///
/// **Snapshot path policy.** `snapshot_path` is owned by the caller
/// — the runner does not invent a path. T6's `apply` orchestrator
/// constructs `migrations/<target>/<app>/schema_snapshot.json` from
/// the workspace root and the bucket; tests pass `None` to skip the
/// snapshot persist step entirely (useful when the test cares only
/// about ledger semantics).
pub struct RunnerCtx {
    /// The bucket this run is applying. Derives the advisory-lock
    /// key and, in production, the snapshot path.
    pub bucket: BucketKey,
    /// The version label (e.g. `V20260425010203__add_users`)
    /// recorded in the ledger.
    pub version: String,
    /// Operator-facing one-line description.
    pub description: String,
    /// Pre-computed checksum_up. The runner recomputes from the
    /// plan's SQL fragments and verifies match before applying.
    /// Format: `V1:<sha256-hex>`.
    pub checksum_up: String,
    /// Pre-computed checksum_down, or `None` when every operation's
    /// down side is a SQL-comment placeholder.
    pub checksum_down: Option<String>,
    /// Snapshot to persist on success. `None` skips the persist step
    /// (used by tests that only care about ledger writes).
    pub snapshot: Option<super::schema::AppliedSchema>,
    /// Where to write the snapshot. Required iff `snapshot.is_some()`.
    pub snapshot_path: Option<PathBuf>,
    /// Migrate-engine config (relpages threshold + strict mode).
    pub config: MigrateConfig,
}

/// Successful-apply report. The runner returns this on a clean
/// `apply_plan` so the caller can log structured progress data.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// `id` of the ledger row this run inserted.
    pub ledger_id: i64,
    /// `run_id` recorded on the ledger row.
    pub run_id: i64,
    /// Number of transactional segments executed.
    pub transactional_segments: usize,
    /// Number of non-transactional segments executed.
    pub non_transactional_segments: usize,
    /// Number of metadata-only segments encountered. T4 records the
    /// segments but does not execute filesystem moves; T6 owns that
    /// path.
    pub metadata_segments: usize,
    /// Wall-clock elapsed time in milliseconds.
    pub execution_time_ms: i64,
}

// ── Public entry point ────────────────────────────────────────────────────

/// Apply a [`MigrationPlan`] against the runner's context.
///
/// **Caller-managed locks.** This function does NOT acquire the
/// workspace file lock — that is the responsibility of the outer
/// orchestrator (T6 `apply` / T5 `repair`) which holds the lock for
/// the entire CLI invocation. The runner DOES acquire and release
/// the per-bucket Postgres advisory lock.
///
/// **Snapshot persistence.** Writes the snapshot file ONLY after
/// the ledger row reaches `applied` and every segment succeeded.
/// Any failure leaves the snapshot at its prior value.
pub async fn apply_plan(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
) -> Result<RunReport, RunnerError> {
    // 1. Bootstrap the ledger table.
    ledger::bootstrap(ctx)
        .await
        .map_err(|e| RunnerError::LedgerWriteFailed {
            version: runner_ctx.version.clone(),
            source: e,
        })?;

    // 2. Acquire pg advisory lock for this bucket.
    let lock_key = advisory_lock_key(&plan.bucket);
    acquire_advisory_lock(ctx, &plan.bucket, lock_key).await?;

    // Whatever happens below, we must release the advisory lock.
    let result = apply_plan_inner(ctx, plan, runner_ctx).await;

    // 9. Always release advisory lock — best effort, log on failure.
    release_advisory_lock(ctx, lock_key).await;

    result
}

/// Internal apply path — keeps the lock-release on the outer
/// function via a deferred call. Returning early from any error
/// path is fine; the caller's `release_advisory_lock` runs after.
async fn apply_plan_inner(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
) -> Result<RunReport, RunnerError> {
    let started = Instant::now();

    // 4. Verify checksum BEFORE inserting the pending row. A
    // mismatch means the plan supplied to the runner does not
    // match the runner_ctx's `checksum_up` — most likely the SQL
    // file was hand-edited.
    let computed_up = compute_checksum_for_plan_up(plan);
    ledger::verify_checksum(&runner_ctx.version, &runner_ctx.checksum_up, &computed_up)
        .map_err(RunnerError::ChecksumMismatch)?;

    // Determine execution_mode + total_steps from the plan shape.
    // `total_steps` counts non-transactional steps (each of which is
    // its own resumable unit). Transactional segments collapse to a
    // single atomic step from the resumability perspective.
    let mut total_non_tx_steps: i32 = 0;
    let mut has_non_tx = false;
    for seg in &plan.segments {
        if seg.kind == SegmentKind::NonTransactional {
            has_non_tx = true;
            total_non_tx_steps = total_non_tx_steps.saturating_add(seg.statements.len() as i32);
        }
    }
    let execution_mode = if has_non_tx {
        ExecutionMode::NonTransactional
    } else {
        ExecutionMode::Transactional
    };

    // 5. Insert pending ledger row. Generate a fresh run_id.
    let run_id = generate_run_id(ctx).await?;
    let ledger_row = LedgerRow {
        version: runner_ctx.version.clone(),
        description: runner_ctx.description.clone(),
        checksum_up: runner_ctx.checksum_up.clone(),
        checksum_down: runner_ctx.checksum_down.clone(),
        execution_mode,
        status: LedgerStatus::Pending,
        execution_time_ms: 0,
        out_of_order_flag: false,
        applied_steps_count: 0,
        total_steps: if has_non_tx {
            Some(total_non_tx_steps)
        } else {
            None
        },
        partial_apply_note: None,
        run_id,
        snapshot_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        app_label: plan.bucket.app.clone(),
    };
    let ledger_id = ledger::insert_pending(ctx, &ledger_row)
        .await
        .map_err(|e| RunnerError::LedgerWriteFailed {
            version: runner_ctx.version.clone(),
            source: e,
        })?;

    // 6. Walk segments. Track counts for the run report.
    let mut transactional_segments = 0usize;
    let mut non_transactional_segments = 0usize;
    let mut metadata_segments = 0usize;
    let mut applied_non_tx_steps: i32 = 0;

    for (seg_idx, segment) in plan.segments.iter().enumerate() {
        match segment.kind {
            SegmentKind::Transactional => {
                if let Err(e) = run_transactional_segment(ctx, segment, runner_ctx).await {
                    let label = match &e {
                        RunnerError::TransactionalSegmentFailed {
                            statement_label, ..
                        } => statement_label.clone(),
                        RunnerError::RelpagesThresholdExceeded { index_name, .. } => {
                            format!("AddIndex {index_name}")
                        }
                        _ => "<unknown>".to_string(),
                    };
                    let note = format!("transactional segment {seg_idx} failed at `{label}`: {e}");
                    let _ = ledger::mark_failed(ctx, ledger_id, &note).await;
                    return Err(map_segment_error(e, seg_idx));
                }
                transactional_segments += 1;
            }
            SegmentKind::NonTransactional => {
                match run_non_transactional_segment(
                    ctx,
                    segment,
                    seg_idx,
                    ledger_id,
                    applied_non_tx_steps,
                )
                .await
                {
                    Ok(steps_completed) => {
                        applied_non_tx_steps = applied_non_tx_steps.saturating_add(steps_completed);
                        non_transactional_segments += 1;
                    }
                    Err(e) => {
                        // mark_partial already recorded inside run_non_transactional_segment
                        // for the per-step failure. We do not double-write; just propagate.
                        return Err(e);
                    }
                }
            }
            SegmentKind::MetadataOnly => {
                // T4 records the segment count but does not execute
                // filesystem moves — that is T6's `apply`
                // orchestrator. The presence of metadata-only
                // segments is captured in the ledger via the count
                // tracked here and returned in the RunReport.
                metadata_segments += 1;
            }
        }
    }

    // 7. Mark applied + persist snapshot. The order is:
    //    a. Write the snapshot file.
    //    b. Mark the ledger row applied.
    //
    // If (a) fails, the ledger stays `pending` and the operator can
    // inspect the partial state. If (b) fails after (a) succeeded,
    // the snapshot is on disk but the ledger says pending — also a
    // recoverable state because the runner's idempotency check
    // (T6) sees the snapshot match the descriptor and treats it as
    // already-applied.
    //
    // However: per the v3 plan's hard invariant ("snapshot moves
    // forward AFTER ledger reaches applied"), we flip the order:
    //    a. Mark the ledger row applied.
    //    b. Write the snapshot file.
    //
    // If (b) fails, the operator runs `compose` to regenerate the
    // snapshot from the descriptor inventory.
    let elapsed_ms: i64 = elapsed_ms(started);

    ledger::mark_applied(ctx, ledger_id, elapsed_ms, applied_non_tx_steps)
        .await
        .map_err(|e| RunnerError::LedgerWriteFailed {
            version: runner_ctx.version.clone(),
            source: e,
        })?;

    if let (Some(snapshot), Some(path)) = (&runner_ctx.snapshot, &runner_ctx.snapshot_path) {
        save_snapshot(snapshot, path).map_err(|e| RunnerError::SnapshotPersistFailed {
            path: path.clone(),
            source: e,
        })?;
    }

    Ok(RunReport {
        ledger_id,
        run_id,
        transactional_segments,
        non_transactional_segments,
        metadata_segments,
        execution_time_ms: elapsed_ms,
    })
}

// ── Segment dispatch helpers ──────────────────────────────────────────────

/// Run every statement inside a transactional segment within a
/// single Postgres transaction. On any error, ROLLBACK and surface
/// the failing statement label.
async fn run_transactional_segment(
    ctx: &mut DjogiContext,
    segment: &Segment,
    runner_ctx: &RunnerCtx,
) -> Result<(), RunnerError> {
    // Probe relpages for any AddIndex statement that does NOT
    // require out-of-transaction. The probe runs BEFORE BEGIN so
    // the abort path on `strict_concurrent_warnings` doesn't leave
    // an open transaction around.
    for stmt in &segment.statements {
        if let Some((index_name, target_table)) = parse_create_index_statement(stmt) {
            relpages_probe(ctx, runner_ctx, &index_name, &target_table).await?;
        }
    }

    ctx.raw_ddl("BEGIN")
        .await
        .map_err(|e| RunnerError::TransactionalSegmentFailed {
            segment_index: 0,
            statement_label: "<BEGIN>".to_string(),
            source: e,
        })?;

    for stmt in &segment.statements {
        if let Err(e) = ctx.raw_ddl(&stmt.up).await {
            // Best-effort rollback — surface the original error
            // regardless of whether the rollback succeeds.
            let _ = ctx.raw_ddl("ROLLBACK").await;
            return Err(RunnerError::TransactionalSegmentFailed {
                segment_index: 0,
                statement_label: stmt.label.clone(),
                source: e,
            });
        }
    }

    ctx.raw_ddl("COMMIT")
        .await
        .map_err(|e| RunnerError::TransactionalSegmentFailed {
            segment_index: 0,
            statement_label: "<COMMIT>".to_string(),
            source: e,
        })?;

    Ok(())
}

/// Run every statement in a non-transactional segment with autocommit.
/// After each successful step, update `applied_steps_count` so
/// crash-recovery (T5 `repair`) can resume from the next step.
///
/// Returns the number of steps completed within this segment so the
/// outer runner can update its running tally of cross-segment progress.
async fn run_non_transactional_segment(
    ctx: &mut DjogiContext,
    segment: &Segment,
    segment_index: usize,
    ledger_id: i64,
    prior_steps_completed: i32,
) -> Result<i32, RunnerError> {
    let mut completed: i32 = 0;
    for (step_idx, stmt) in segment.statements.iter().enumerate() {
        if let Err(e) = ctx.raw_ddl(&stmt.up).await {
            let total_so_far = prior_steps_completed.saturating_add(completed);
            let note = format!(
                "non-tx step {step} of segment {seg} failed: {label} — {e}",
                step = step_idx + 1,
                seg = segment_index,
                label = stmt.label,
            );
            // Best-effort partial-state record. If the ledger update
            // itself fails, we still surface the original step
            // failure — the partial-state record is forensic only.
            let _ = ledger::mark_partial(ctx, ledger_id, total_so_far, &note).await;
            return Err(RunnerError::NonTransactionalSegmentFailed {
                segment_index,
                step_index: step_idx,
                statement_label: stmt.label.clone(),
                applied_steps_count: total_so_far,
                source: e,
            });
        }
        completed = completed.saturating_add(1);
        let total_so_far = prior_steps_completed.saturating_add(completed);
        // Best-effort progress update. Failure here does not abort
        // the segment — the SQL already ran; recording it is a
        // bookkeeping concern.
        if let Err(e) = ledger::update_progress(ctx, ledger_id, total_so_far).await {
            tracing::warn!(
                ledger_id,
                applied_steps = total_so_far,
                error = ?e,
                "ledger progress update failed; SQL already applied",
            );
        }
    }
    Ok(completed)
}

// ── Advisory lock + relpages probe ────────────────────────────────────────

/// Derive a stable 64-bit advisory-lock key from a `BucketKey`. Uses
/// SHA-256 truncated to the low 64 bits for a fixed-seed hash —
/// stdlib `Hasher` is randomised per process and cannot be used.
///
/// Format: `SHA256("djogi:" || database || "\0" || app)` then take
/// the first 8 bytes interpreted as little-endian `u64`, then
/// reinterpret as `i64` (Postgres `bigint`). The `djogi:` prefix
/// scopes the keyspace so we cannot collide with adopter-side
/// advisory locks that hash arbitrary identifiers.
pub fn advisory_lock_key(bucket: &BucketKey) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(b"djogi:advisory_lock:");
    hasher.update(bucket.database.as_bytes());
    hasher.update(b"\x00");
    hasher.update(bucket.app.as_bytes());
    let digest = hasher.finalize();
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest[..8]);
    i64::from_le_bytes(buf)
}

/// Acquire a Postgres advisory lock on `key`. Postgres
/// `pg_advisory_lock(bigint)` blocks indefinitely; we use
/// `pg_try_advisory_lock(bigint)` in a bounded retry loop so a
/// stuck holder cannot wedge the runner.
async fn acquire_advisory_lock(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
    key: i64,
) -> Result<(), RunnerError> {
    const MAX_ATTEMPTS: u32 = 600; // 600 * 50 ms = 30 s
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    for attempt in 0..MAX_ATTEMPTS {
        let row = ctx
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .await
            .map_err(|e| RunnerError::LedgerWriteFailed {
                version: bucket.app.clone(),
                source: e,
            })?;
        let acquired: bool = row.try_get(0).map_err(|e| RunnerError::LedgerWriteFailed {
            version: bucket.app.clone(),
            source: DjogiError::from(e),
        })?;
        if acquired {
            return Ok(());
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
        if attempt + 1 == MAX_ATTEMPTS {
            return Err(RunnerError::AdvisoryLockFailed {
                bucket: bucket.clone(),
                key,
                attempts: MAX_ATTEMPTS,
            });
        }
    }
    // Unreachable in practice — the loop body returns on every
    // iteration. This is a defensive fallback to keep the function
    // total against future loop edits.
    Err(RunnerError::AdvisoryLockFailed {
        bucket: bucket.clone(),
        key,
        attempts: MAX_ATTEMPTS,
    })
}

/// Release a previously-acquired advisory lock. Best-effort —
/// logs on failure but does not surface the error because the
/// runner is on its way out.
async fn release_advisory_lock(ctx: &mut DjogiContext, key: i64) {
    if let Err(e) = ctx.execute("SELECT pg_advisory_unlock($1)", &[&key]).await {
        tracing::warn!(
            ?e,
            key,
            "pg_advisory_unlock failed; lock will be released on session close"
        );
    }
}

/// Run the relpages probe before a transactional `CREATE INDEX`.
/// On WARN path: emit `tracing::warn!` and continue. On strict path
/// (`migrate.strict_concurrent_warnings = true`): surface
/// `RunnerError::RelpagesThresholdExceeded`.
async fn relpages_probe(
    ctx: &mut DjogiContext,
    runner_ctx: &RunnerCtx,
    index_name: &str,
    target_table: &str,
) -> Result<(), RunnerError> {
    let row_opt = ctx
        .query_opt(
            "SELECT relpages FROM pg_class WHERE relname = $1 AND relkind = 'r'",
            &[&target_table],
        )
        .await
        .map_err(|e| RunnerError::LedgerWriteFailed {
            version: index_name.to_string(),
            source: e,
        })?;
    let relpages: i32 = match row_opt {
        Some(r) => r
            .try_get::<_, i32>(0)
            .map_err(|e| RunnerError::LedgerWriteFailed {
                version: index_name.to_string(),
                source: DjogiError::from(e),
            })?,
        // Table not found in pg_class — typically a CREATE INDEX on
        // a table created by a preceding statement in the same
        // segment that has not yet been committed. The relpages of a
        // table not yet visible to this transaction is undefined; we
        // pessimistically treat it as 0 (no warning) because the
        // table is by definition new and has no existing rows that
        // an ACCESS EXCLUSIVE lock would block.
        None => 0,
    };
    let threshold = runner_ctx.config.concurrent_warn_relpages;
    if (relpages as i64) > (threshold as i64) {
        if runner_ctx.config.strict_concurrent_warnings {
            return Err(RunnerError::RelpagesThresholdExceeded {
                bucket: runner_ctx.bucket.clone(),
                index_name: index_name.to_string(),
                target_table: target_table.to_string(),
                relpages,
                threshold,
            });
        } else {
            tracing::warn!(
                bucket_database = %runner_ctx.bucket.database,
                bucket_app = %runner_ctx.bucket.app,
                index_name,
                target_table,
                relpages,
                threshold,
                "transactional CREATE INDEX on a large table will hold ACCESS EXCLUSIVE \
                 for the duration; consider opting into CREATE INDEX CONCURRENTLY",
            );
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Compute the `up`-side checksum across every segment's statements.
/// Concatenates each statement's `up` SQL with `\n` separators in
/// segment-then-statement order. Mirrors the runner_ctx pre-compute
/// path so the two sides match by construction.
fn compute_checksum_for_plan_up(plan: &MigrationPlan) -> String {
    let fragments: Vec<&str> = plan
        .segments
        .iter()
        .flat_map(|s| s.statements.iter())
        .map(|s| s.up.as_str())
        .collect();
    compute_checksum(fragments)
}

/// Lift an `OperationSql` into a `(index_name, target_table)` pair if
/// the statement is a transactional `CREATE INDEX`. Returns `None`
/// for any other operation.
///
/// The label format is set by the SQL emitter — `AddIndex <name>`
/// for new indexes (see `sql.rs::emit_add_index`). The table name is
/// recovered from the SQL's `ON "<table>"` clause via byte-level
/// scanning (no regex).
fn parse_create_index_statement(stmt: &OperationSql) -> Option<(String, String)> {
    // Only AddIndex labels are eligible. The DropIndex labels start
    // with "DropIndex" so they cannot collide.
    let label = stmt.label.as_str();
    let index_name = label.strip_prefix("AddIndex ")?.to_string();

    // Extract the table name by scanning for the literal ` ON "`
    // marker followed by the quoted table name. Postgres `CREATE
    // INDEX` SQL always emits `ON "<table>"` — see emit_add_index.
    // No regex; byte-level forward scan.
    let bytes = stmt.up.as_bytes();
    let needle = b" ON \"";
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let start = i + needle.len();
            // Find the closing quote.
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            if j > start && j < bytes.len() {
                // SAFETY: we restricted the byte range to
                // identifier characters (Postgres double-quoted
                // identifier — ASCII alphanumerics + underscores +
                // some punctuation; never invalid UTF-8 in practice
                // because identifiers are ASCII). Falling back to
                // the lossy converter keeps this fn total.
                let table = String::from_utf8_lossy(&bytes[start..j]).into_owned();
                return Some((index_name, table));
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Translate the inner segment-failure error into the public
/// runner-error variant, threading the segment index forward.
fn map_segment_error(e: RunnerError, segment_index: usize) -> RunnerError {
    match e {
        RunnerError::TransactionalSegmentFailed {
            statement_label,
            source,
            ..
        } => RunnerError::TransactionalSegmentFailed {
            segment_index,
            statement_label,
            source,
        },
        other => other,
    }
}

/// Generate a fresh `run_id` via the HeerId default-allocation path.
/// HeerId is a 64-bit time-ordered ID — perfect for the per-runner
/// invocation key, which we want to be unique, sortable, and stable
/// across machines.
async fn generate_run_id(ctx: &mut DjogiContext) -> Result<i64, RunnerError> {
    use crate::primary_key::PrimaryKeyDbGen;
    let id = HeerId::generate(ctx)
        .await
        .map_err(|e| RunnerError::LedgerWriteFailed {
            version: "<run_id>".to_string(),
            source: e,
        })?;
    // HeerId is a 64-bit value; reinterpret its bytes as i64 for
    // the BIGINT column. The wire encoding is identical because
    // Postgres treats BIGINT as i64 and HeerId's ToSql impl writes
    // the same 8 bytes either way.
    Ok(heerid_to_i64(id))
}

/// Reinterpret a `HeerId` as an `i64` for the ledger's BIGINT
/// column. The two values share the same wire bytes; we go via the
/// 8-byte buffer to avoid depending on internal HeerId fields.
fn heerid_to_i64(id: HeerId) -> i64 {
    // HeerId::Display is the canonical decimal printing — we route
    // through u64::from_str_radix on the unsigned form so leading
    // bits map cleanly. heeranjid 0.3 exposes `as_i64` via the
    // postgres-types ToSql impl; we cannot reach that helper from
    // here without a workspace dependency change, so a parse from
    // the canonical Display form keeps this self-contained.
    //
    // Display always emits the canonical signed-bigint form for
    // HeerId, so parsing always succeeds for genuine values. A
    // failure here means heeranjid changed its Display impl;
    // surface a stable fallback (`0`) rather than panicking the
    // runner.
    let s = format!("{id}");
    s.parse::<i64>().unwrap_or(0)
}

fn elapsed_ms(t0: Instant) -> i64 {
    t0.elapsed().as_millis().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::diff::Classification;
    use crate::migrate::projection::BucketKey;
    use crate::migrate::segment::{MigrationPlan, Segment, SegmentKind};
    use crate::migrate::sql::OperationSql;

    fn bucket(db: &str, app: &str) -> BucketKey {
        BucketKey {
            database: db.to_string(),
            app: app.to_string(),
        }
    }

    // ── advisory_lock_key determinism ────────────────────────────────────

    #[test]
    fn advisory_lock_key_is_deterministic_across_calls() {
        let b = bucket("main", "");
        let a = advisory_lock_key(&b);
        let c = advisory_lock_key(&b);
        assert_eq!(a, c, "same input must yield same lock key");
    }

    #[test]
    fn advisory_lock_key_differs_on_database() {
        let a = advisory_lock_key(&bucket("alpha", ""));
        let b = advisory_lock_key(&bucket("beta", ""));
        assert_ne!(a, b, "different database must yield different key");
    }

    #[test]
    fn advisory_lock_key_differs_on_app() {
        let a = advisory_lock_key(&bucket("main", "users"));
        let b = advisory_lock_key(&bucket("main", "billing"));
        assert_ne!(a, b, "different app must yield different key");
    }

    #[test]
    fn advisory_lock_key_database_app_separator_prevents_collision() {
        // A naive concat would collide:
        //   ("ab", "c")  →  "abc"
        //   ("a", "bc")  →  "abc"
        // The `\0` separator means the two cannot collide.
        let a = advisory_lock_key(&bucket("ab", "c"));
        let b = advisory_lock_key(&bucket("a", "bc"));
        assert_ne!(a, b);
    }

    // ── parse_create_index_statement ─────────────────────────────────────

    #[test]
    fn parse_index_extracts_name_and_table() {
        let stmt = OperationSql {
            label: "AddIndex users_email_idx".to_string(),
            up: "CREATE INDEX \"users_email_idx\" ON \"users\" (\"email\")".to_string(),
            down: String::new(),
            lossy: None,
        };
        let parsed = parse_create_index_statement(&stmt).expect("parse");
        assert_eq!(parsed.0, "users_email_idx");
        assert_eq!(parsed.1, "users");
    }

    #[test]
    fn parse_index_returns_none_for_non_index_label() {
        let stmt = OperationSql {
            label: "AddTable users".to_string(),
            up: "CREATE TABLE \"users\" ()".to_string(),
            down: String::new(),
            lossy: None,
        };
        assert!(parse_create_index_statement(&stmt).is_none());
    }

    #[test]
    fn parse_index_returns_none_when_marker_missing() {
        let stmt = OperationSql {
            label: "AddIndex weird_idx".to_string(),
            up: "CREATE INDEX weird".to_string(),
            down: String::new(),
            lossy: None,
        };
        assert!(parse_create_index_statement(&stmt).is_none());
    }

    // ── compute_checksum_for_plan_up ─────────────────────────────────────

    #[test]
    fn plan_checksum_matches_manual_concatenation() {
        let plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![
                    OperationSql {
                        label: "AddTable a".to_string(),
                        up: "CREATE TABLE a ()".to_string(),
                        down: "DROP TABLE a".to_string(),
                        lossy: None,
                    },
                    OperationSql {
                        label: "AddTable b".to_string(),
                        up: "CREATE TABLE b ()".to_string(),
                        down: "DROP TABLE b".to_string(),
                        lossy: None,
                    },
                ],
            }],
        };
        let computed = compute_checksum_for_plan_up(&plan);
        let manual = compute_checksum(["CREATE TABLE a ()", "CREATE TABLE b ()"]);
        assert_eq!(computed, manual);
    }

    #[test]
    fn plan_checksum_changes_on_segment_reorder() {
        let make = |labels: [&str; 2]| MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: labels
                    .iter()
                    .map(|l| OperationSql {
                        label: format!("AddTable {l}"),
                        up: format!("CREATE TABLE {l} ()"),
                        down: format!("DROP TABLE {l}"),
                        lossy: None,
                    })
                    .collect(),
            }],
        };
        let a = compute_checksum_for_plan_up(&make(["a", "b"]));
        let b = compute_checksum_for_plan_up(&make(["b", "a"]));
        assert_ne!(a, b);
    }

    // ── elapsed_ms ────────────────────────────────────────────────────────

    #[test]
    fn elapsed_ms_returns_non_negative() {
        let t = Instant::now();
        let ms = elapsed_ms(t);
        assert!(ms >= 0);
    }

    // ── heerid_to_i64 ─────────────────────────────────────────────────────

    #[test]
    fn heerid_to_i64_zero_round_trips() {
        let z = HeerId::ZERO;
        let v = heerid_to_i64(z);
        // ZERO renders as "0" in canonical decimal form.
        assert_eq!(v, 0);
    }
}
