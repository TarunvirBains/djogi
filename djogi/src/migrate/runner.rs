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

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::config::MigrateConfig;
use crate::context::DjogiContext;
use crate::error::{DbError, DjogiError};
use crate::types::HeerId;

use super::guard::WorkspaceGuard;
use super::ledger::{
    self, ChecksumFormatError, ChecksumMismatch, ExecutionMode, LedgerRow, LedgerStatus,
    VerifyError, compute_checksum,
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

    /// One of the two checksum strings handed to the runner failed
    /// [`super::ledger::validate_checksum_format`]. The wrapped error
    /// identifies which side (expected vs. actual) was malformed and
    /// the rule violated.
    ChecksumFormat(ChecksumFormatError),

    /// A ledger row CRUD operation failed (INSERT, UPDATE).
    LedgerWriteFailed { version: String, source: DjogiError },

    /// Insertion of the pending ledger row collided with an existing
    /// row carrying the same `version`. Surfaces a typed error rather
    /// than a raw `LedgerWriteFailed { source: 23505 }` so operators
    /// re-running an already-applied migration get an actionable
    /// message that names the prior `applied_at` timestamp.
    VersionAlreadyApplied {
        version: String,
        applied_at: Option<OffsetDateTime>,
    },

    /// The relpages probe queried `pg_class` for a target table that
    /// did NOT match any AddTable in the current plan and the table
    /// was not present in the database. This catches typos / mis-
    /// quoted identifiers — silently dropping to `relpages = 0` would
    /// disable the strict-mode warning path for any mis-targeted
    /// `CREATE INDEX`.
    TargetTableNotFound {
        bucket: BucketKey,
        index_name: String,
        target_table: String,
    },

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
            RunnerError::ChecksumFormat(e) => write!(f, "{e}"),
            RunnerError::LedgerWriteFailed { version, source } => {
                write!(f, "ledger write failed for version `{version}`: {source}")
            }
            RunnerError::VersionAlreadyApplied {
                version,
                applied_at,
            } => match applied_at {
                Some(when) => write!(
                    f,
                    "migration version `{version}` was already applied at {when}; \
                     re-running is rejected — use `djogi migrations status` to confirm",
                ),
                None => write!(
                    f,
                    "migration version `{version}` was already applied; \
                     re-running is rejected — use `djogi migrations status` to confirm",
                ),
            },
            RunnerError::TargetTableNotFound {
                bucket,
                index_name,
                target_table,
            } => write!(
                f,
                "relpages probe for `{index}` could not locate target table `{table}` \
                 (bucket database={db} app={app}); the index plan does not create \
                 this table either — check for a typo or a mis-quoted identifier",
                index = index_name,
                table = target_table,
                db = bucket.database,
                app = bucket.app,
            ),
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
            RunnerError::ChecksumFormat(e) => Some(e),
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
/// **Witness-typed workspace lock.** The `_guard: &WorkspaceGuard`
/// parameter is a compile-time witness that the caller already holds
/// the workspace file lock — its mere presence at the type level
/// proves the lock is alive for the duration of the call. The runner
/// itself does not touch the guard; the parameter is named with a
/// leading underscore to signal "consumed at the type level only".
///
/// Misuse — calling `apply_plan` without first acquiring the lock —
/// is a compile error rather than a silent race window. Tests that
/// only care about ledger semantics still go through the lock by
/// asking the [`super::guard::acquire`] helper for a per-test path.
///
/// **Three-database awareness.** The runner currently routes every
/// query through the supplied `&mut DjogiContext`'s pool. The
/// `RunnerCtx::bucket.database` field is hashed into the advisory
/// lock key and surfaced in operator-facing errors, but the actual
/// query routing follows the context the caller supplied. Phase 4's
/// `DjogiContext` is single-pool today; when the three-database
/// `DjogiContext::pool_for(database)` API lands the runner will
/// pull the right pool from `runner_ctx.bucket.database` here. The
/// `BucketKey.database` channel exists today so the orchestrator
/// (T6 `apply`) can construct one `DjogiContext` per database before
/// invoking the runner.
///
/// **Per-bucket advisory lock.** The runner DOES acquire and
/// release the per-bucket Postgres advisory lock around every
/// segment dispatch.
///
/// **Snapshot persistence.** Writes the snapshot file ONLY after
/// the ledger row reaches `applied` and every segment succeeded.
/// Any failure leaves the snapshot at its prior value.
pub async fn apply_plan(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    _guard: &WorkspaceGuard,
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
    // file was hand-edited. A FormatError means one of the inputs
    // was not a well-formed `V1:<sha256-hex>` string and is also
    // a hard error: we never want to fall through to the byte
    // compare with malformed inputs.
    let computed_up = compute_checksum_for_plan_up(plan);
    if let Err(e) =
        ledger::verify_checksum(&runner_ctx.version, &runner_ctx.checksum_up, &computed_up)
    {
        return Err(match e {
            VerifyError::Mismatch(m) => RunnerError::ChecksumMismatch(m),
            VerifyError::Format(f) => RunnerError::ChecksumFormat(f),
        });
    }

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
    let ledger_id = match ledger::insert_pending(ctx, &ledger_row).await {
        Ok(id) => id,
        Err(e) => {
            // SQLSTATE 23505 (unique_violation) on the
            // `djogi_schema_migrations.version` column means the
            // operator is re-running an already-applied migration.
            // Lift the raw PG error into a typed
            // `VersionAlreadyApplied` so the message names the prior
            // `applied_at` rather than dumping a generic CRUD failure.
            if is_unique_violation(&e) {
                let applied_at = load_applied_at(ctx, &runner_ctx.version).await;
                return Err(RunnerError::VersionAlreadyApplied {
                    version: runner_ctx.version.clone(),
                    applied_at,
                });
            }
            return Err(RunnerError::LedgerWriteFailed {
                version: runner_ctx.version.clone(),
                source: e,
            });
        }
    };

    // 6. Walk segments. Track counts for the run report.
    let mut transactional_segments = 0usize;
    let mut non_transactional_segments = 0usize;
    let mut metadata_segments = 0usize;
    let mut applied_non_tx_steps: i32 = 0;

    // Build the `AddTable` set from the plan. The relpages probe
    // uses this to disambiguate "table being created in this same
    // plan" (legit `relpages = None`) from "typo / mis-quoted
    // identifier" (hard `TargetTableNotFound`).
    let add_table_set = collect_add_table_targets(plan);

    for (seg_idx, segment) in plan.segments.iter().enumerate() {
        match segment.kind {
            SegmentKind::Transactional => {
                if let Err(e) =
                    run_transactional_segment(ctx, segment, runner_ctx, &add_table_set).await
                {
                    // N-1: the relpages probe runs BEFORE BEGIN, so
                    // a probe failure must NOT be reported as a
                    // transactional-segment failure (the tx has not
                    // even opened yet). Distinguish probe-side
                    // failures from in-tx statement failures so the
                    // operator-facing note matches the actual phase
                    // that failed.
                    let note = match &e {
                        RunnerError::RelpagesThresholdExceeded {
                            index_name,
                            target_table,
                            relpages,
                            threshold,
                            ..
                        } => format!(
                            "relpages-probe failed at AddIndex {index_name} on table \
                             {target_table} (relpages={relpages} > threshold={threshold})",
                        ),
                        RunnerError::TargetTableNotFound {
                            index_name,
                            target_table,
                            ..
                        } => format!(
                            "relpages-probe failed at AddIndex {index_name}: target table \
                             `{target_table}` not found and not in plan's AddTable set",
                        ),
                        RunnerError::TransactionalSegmentFailed {
                            statement_label,
                            source,
                            ..
                        } => format!(
                            "transactional segment {seg_idx} failed at `{statement_label}`: \
                             {source}",
                        ),
                        other => format!("transactional segment {seg_idx} failed: {other}",),
                    };
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
    add_table_set: &BTreeSet<String>,
) -> Result<(), RunnerError> {
    // Probe relpages for any AddIndex statement that does NOT
    // require out-of-transaction. The probe runs BEFORE BEGIN so
    // the abort path on `strict_concurrent_warnings` doesn't leave
    // an open transaction around.
    for stmt in &segment.statements {
        if let Some((index_name, target_table)) = parse_create_index_statement(stmt) {
            relpages_probe(ctx, runner_ctx, &index_name, &target_table, add_table_set).await?;
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
/// **Byte order: big-endian.** The Phase 7 v3 contract pins
/// big-endian decoding of the first 8 SHA-256 digest bytes so an
/// alternate-language implementation following the same spec
/// computes the identical `i64` and contends correctly with this
/// runner. Network byte order is the spec; little-endian would key
/// the same bucket to a different value across implementations.
///
/// Format: `SHA256("djogi:advisory_lock:" || database || "\0" || app)`,
/// then take the first 8 digest bytes as a big-endian signed 64-bit
/// integer (Postgres `bigint`). The `djogi:advisory_lock:` prefix
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
    i64::from_be_bytes(buf)
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
///
/// **`pg_class.relpages = None` disambiguation.** A `None` row from
/// the probe means Postgres does not currently know about the table.
/// Two legitimate cases produce that:
///
/// 1. The current plan creates the table in an earlier segment of
///    THIS run (`AddTable` statement). The runner treats this as
///    `relpages = 0` (a freshly-created empty table cannot exceed
///    the threshold).
/// 2. The table genuinely does not exist and is not being created.
///    That is a typo / mis-quoted identifier — the index would fail
///    inside `BEGIN` anyway, but the strict-mode probe catches it
///    earlier with a clearer `TargetTableNotFound` diagnostic.
///
/// Silently dropping case 2 to `relpages = 0` would disable the
/// strict warning path for any mis-targeted `CREATE INDEX`, which
/// is exactly the failure mode strict-mode exists to catch.
async fn relpages_probe(
    ctx: &mut DjogiContext,
    runner_ctx: &RunnerCtx,
    index_name: &str,
    target_table: &str,
    add_table_set: &BTreeSet<String>,
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
        // Table not found in pg_class — distinguish the two legitimate
        // cases (see fn doc).
        None => {
            if add_table_set.contains(target_table) {
                // Case 1: table is being CREATEd in this same plan;
                // relpages of a yet-uncommitted CREATE TABLE is
                // undefined. Treat as 0 so the warn path does not
                // fire.
                0
            } else {
                // Case 2: hard error. The plan does not create this
                // table and Postgres does not know about it.
                return Err(RunnerError::TargetTableNotFound {
                    bucket: runner_ctx.bucket.clone(),
                    index_name: index_name.to_string(),
                    target_table: target_table.to_string(),
                });
            }
        }
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
    // HeerId exposes a direct `as_i64()` accessor (and an equivalent
    // `From<HeerId> for i64` impl). Use the typed conversion rather
    // than routing through `Display + parse + unwrap_or(0)` so a
    // misbehaving Display impl cannot collapse a real ID to `0`.
    Ok(id.as_i64())
}

fn elapsed_ms(t0: Instant) -> i64 {
    t0.elapsed().as_millis().min(i64::MAX as u128) as i64
}

/// Return `true` iff `e` carries Postgres SQLSTATE 23505
/// (unique_violation). Used by `apply_plan` to lift the raw
/// duplicate-version insert error into the typed
/// `RunnerError::VersionAlreadyApplied` variant.
fn is_unique_violation(e: &DjogiError) -> bool {
    use tokio_postgres::error::SqlState;
    match e {
        DjogiError::Db(db) => db_code_matches(db, &SqlState::UNIQUE_VIOLATION),
        _ => false,
    }
}

/// Inspect a `DbError`'s SQLSTATE without reaching across the
/// `DbError` opaque boundary directly. The accessor returns
/// `Option<&SqlState>`; we compare by reference.
fn db_code_matches(db: &DbError, target: &tokio_postgres::error::SqlState) -> bool {
    db.code().map(|c| c == target).unwrap_or(false)
}

/// Read the `applied_at` timestamp of an existing row whose
/// `version` we just collided with. Returns `None` if the lookup
/// fails — the operator-facing message degrades gracefully because
/// the message is informational only.
async fn load_applied_at(ctx: &mut DjogiContext, version: &str) -> Option<OffsetDateTime> {
    let row = ctx
        .query_opt(
            "SELECT applied_at FROM djogi_schema_migrations WHERE version = $1",
            &[&version],
        )
        .await
        .ok()??;
    row.try_get::<_, OffsetDateTime>("applied_at").ok()
}

/// Walk the plan and collect the set of table names that an
/// `AddTable` statement creates. The relpages probe uses this to
/// disambiguate "table being created in this very plan" (legit
/// `relpages = None`) from "typo / mis-quoted identifier"
/// (`TargetTableNotFound`).
fn collect_add_table_targets(plan: &MigrationPlan) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for seg in &plan.segments {
        for stmt in &seg.statements {
            if let Some(table) = stmt.label.strip_prefix("AddTable ") {
                out.insert(table.to_string());
            }
        }
    }
    out
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
        //   ("ab", "c")  ->  "abc"
        //   ("a", "bc")  ->  "abc"
        // The `\0` separator means the two cannot collide.
        let a = advisory_lock_key(&bucket("ab", "c"));
        let b = advisory_lock_key(&bucket("a", "bc"));
        assert_ne!(a, b);
    }

    #[test]
    fn advisory_lock_key_pins_big_endian_byte_decode() {
        // Spec-pinned reference value. The test exists to lock in the
        // big-endian decode of the first 8 SHA-256 digest bytes per
        // the Phase 7 v3 contract — an alternate-language
        // implementation following the same spec must compute the
        // same i64 for these inputs.
        //
        // Reference computation (also verified offline with Python):
        //   d = sha256(b"djogi:advisory_lock:" + b"test_db" + b"\x00" + b"test_app")
        //   bytes[..8] = 7d 01 ce 8f 30 91 ba 37
        //   big-endian i64 = 0x7d01ce8f3091ba37 = 9_007_707_844_108_204_599
        //   little-endian (the WRONG decode) would produce
        //                                     4_015_681_655_511_318_909.
        let bk = bucket("test_db", "test_app");
        let key = advisory_lock_key(&bk);
        assert_eq!(
            key, 9_007_707_844_108_204_599_i64,
            "advisory_lock_key must decode the first 8 SHA-256 bytes as big-endian"
        );
        // Negative-control: confirm we are NOT little-endian decoding.
        assert_ne!(
            key, 4_015_681_655_511_318_909_i64,
            "advisory_lock_key must NOT decode bytes as little-endian"
        );
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

    // ── HeerId direct conversion ─────────────────────────────────────────

    #[test]
    fn heer_id_zero_converts_to_zero_i64_directly() {
        // After A-3, run_id derivation goes through HeerId::as_i64
        // / From<HeerId> for i64 — no Display + parse + unwrap_or
        // detour. ZERO's i64 representation must be 0 by both paths.
        let z = HeerId::ZERO;
        assert_eq!(z.as_i64(), 0);
        let via_from: i64 = i64::from(z);
        assert_eq!(via_from, 0);
    }

    // ── collect_add_table_targets ────────────────────────────────────────

    #[test]
    fn collect_add_table_targets_walks_all_segments() {
        let plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![
                Segment {
                    kind: SegmentKind::Transactional,
                    statements: vec![
                        OperationSql {
                            label: "AddTable users".to_string(),
                            up: "CREATE TABLE users ()".to_string(),
                            down: "DROP TABLE users".to_string(),
                            lossy: None,
                        },
                        OperationSql {
                            label: "AddIndex users_email_idx".to_string(),
                            up: "CREATE INDEX...".to_string(),
                            down: String::new(),
                            lossy: None,
                        },
                    ],
                },
                Segment {
                    kind: SegmentKind::Transactional,
                    statements: vec![OperationSql {
                        label: "AddTable orders".to_string(),
                        up: "CREATE TABLE orders ()".to_string(),
                        down: "DROP TABLE orders".to_string(),
                        lossy: None,
                    }],
                },
            ],
        };
        let set = collect_add_table_targets(&plan);
        assert!(set.contains("users"));
        assert!(set.contains("orders"));
        assert!(!set.contains("users_email_idx")); // AddIndex is not AddTable
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn collect_add_table_targets_empty_plan_returns_empty_set() {
        let plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::NoOp,
            segments: vec![],
        };
        let set = collect_add_table_targets(&plan);
        assert!(set.is_empty());
    }

    // ── is_unique_violation classifier ───────────────────────────────────

    #[test]
    fn is_unique_violation_rejects_non_db_errors() {
        // Anything that is not a DjogiError::Db must classify as
        // false — only a Db error carries a SQLSTATE.
        let nf = DjogiError::not_found("users");
        assert!(!is_unique_violation(&nf));
    }
}
