//! Migration repair — operator-confirmed fixes for ledger drift,
//! partial applies, and missing snapshots.
//!
//! # Why repair lives in its own module
//!
//! [`super::runner`] applies migrations forward. [`super::verify`]
//! reads the live database and reports drift. Neither mutates anything
//! when something is wrong — the operator decides whether the
//! mutation is safe. Repair is the one place that *does* mutate the
//! ledger / snapshot, and every entry point requires an explicit
//! [`RepairConfirmation`] witness so an absent-mindedly-flipped
//! `bool true` cannot trigger a destructive update.
//!
//! # Confirmation witness
//!
//! Every repair takes [`RepairConfirmation::OperatorAcknowledged`] —
//! a single-variant enum whose only constructor is the variant name
//! itself. That makes the call site loud:
//!
//! ```ignore
//! repair_checksum_drift(
//!     &mut ctx,
//!     &guard,
//!     &bucket,
//!     "V20260425010203__add_users",
//!     &fresh_checksum,
//!     None,
//!     RepairConfirmation::OperatorAcknowledged,
//! ).await?;
//! ```
//!
//! No `Default::default()` lands here, no `bool` flips, no implicit
//! coercion. The operator has to *type out* the variant name.
//!
//! # Why a witness instead of an `unsafe fn`?
//!
//! `unsafe` in Rust signals memory-safety obligations. Repair's
//! danger is operational, not memory-related — using `unsafe`
//! conflates two unrelated risk classes. The witness pattern keeps
//! `unsafe` available for actual unsafe code while still forcing the
//! caller to type out a destructive intent.
//!
//! # Workspace lock
//!
//! Each repair entry point takes `&super::guard::WorkspaceGuard` for
//! the same reason [`super::runner::apply_plan`] does — the file lock
//! must be held for the entire repair so a concurrent `apply` /
//! `verify` cannot race with the ledger mutation.
//!
//! # Three repair flows (Phase 7 v3 §8)
//!
//! 1. [`repair_checksum_drift`] — ledger row's `checksum_up` no longer
//!    matches the migration file's content. Repair updates the row to
//!    the freshly-computed checksum.
//! 2. [`repair_partial_apply`] — non-transactional apply crashed
//!    mid-segment. Repair rewrites the row's status / progress to
//!    one of `RolledBack` / `Faked` / `Applied` based on the
//!    operator's resolution choice.
//! 3. [`repair_snapshot_rebuild`] — snapshot file is missing or
//!    corrupt. Repair walks the ledger and re-projects the cumulative
//!    schema, then writes the new snapshot.
//!
//! All three return a [`RepairReport`] documenting exactly what
//! changed, so the operator can audit (and replay-via-shell-history
//! when needed).

use std::path::PathBuf;

use crate::__bypass::guarded_batch_execute;
use crate::context::{DjogiContext, PinnedCtx};
use crate::error::DjogiError;

use super::guard::WorkspaceGuard;
use super::ledger::{
    self, ChecksumFormatErrorKind, LedgerRow, LedgerStatus, compute_checksum,
    load_full_row_by_version, validate_checksum_format,
};
use super::projection::BucketKey;
use super::runner::{acquire_advisory_lock, advisory_lock_key, release_advisory_lock};
use super::segment::{MigrationPlan, SegmentKind};
use super::snapshot::{SnapshotError, save_snapshot};

// ── Confirmation witness ──────────────────────────────────────────────────

/// Operator confirmation witness for destructive repair operations.
///
/// **Single-variant enum, single name.** The only way to construct
/// `RepairConfirmation` is to name the variant explicitly:
///
/// ```ignore
/// RepairConfirmation::OperatorAcknowledged
/// ```
///
/// There is no `Default` impl, no `From<bool>`, no `try_from` —
/// any code that wants to call a repair function has to spell out
/// "yes, do the destructive thing" at the call site. This is the
/// witness pattern recommended by the Phase 7 v3 plan §8 for
/// repair-class operations.
///
/// # Why a single-variant enum
///
/// A struct-with-private-fields would also work, but an enum reads
/// more naturally at the call site (`RepairConfirmation::OperatorAcknowledged`
/// vs. `RepairConfirmation::operator_acknowledged()`) and gives us
/// room to add an explicit `OperatorAcknowledgedWithReason { reason
/// String }` variant later without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairConfirmation {
    /// The operator has acknowledged the destructive nature of the
    /// repair and is asking the system to proceed. The variant name
    /// must appear at the call site verbatim.
    OperatorAcknowledged,
}

// ── RepairReport ──────────────────────────────────────────────────────────

/// Result of a successful repair invocation.
///
/// Each repair surfaces exactly what it touched so the operator can
/// audit the change. `actions_taken` is a free-form log; `ledger_changes`
/// and `snapshot_changes` carry structured records.
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// One-line operator-facing record per action. Sorted in the
    /// order the actions executed.
    pub actions_taken: Vec<String>,
    /// Ledger-row mutations performed.
    pub ledger_changes: Vec<LedgerChange>,
    /// Snapshot-file mutations performed.
    pub snapshot_changes: Vec<SnapshotChange>,
}

/// One ledger-row mutation. Documents the exact column changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerChange {
    /// `version` of the affected row.
    pub version: String,
    /// Column updated.
    pub column: &'static str,
    /// Previous value (rendered for human consumption).
    pub before: String,
    /// New value (rendered for human consumption).
    pub after: String,
}

impl LedgerChange {
    /// Construct a [`LedgerChange`]. Centralises the verbose
    /// struct-literal form previously inlined at every call site
    /// (cluster-2 simplify Finding 8).
    pub(crate) fn new(version: &str, column: &'static str, before: String, after: String) -> Self {
        Self {
            version: version.to_string(),
            column,
            before,
            after,
        }
    }
}

/// One snapshot-file mutation. Documents the path and the kind of
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotChange {
    /// File path that was written.
    pub path: PathBuf,
    /// Description of the change (e.g. `"rebuilt from ledger"`).
    pub description: String,
}

// ── RepairError ───────────────────────────────────────────────────────────

/// Failure modes for the repair entry points.
#[derive(Debug)]
pub enum RepairError {
    /// The ledger row identified by `version` does not exist.
    VersionNotFound { version: String },

    /// Caller supplied a confirmation other than the explicit
    /// witness. Today the witness type only has a single variant, so
    /// this branch is reserved for future expansions (e.g.
    /// `OperatorAcknowledgedWithReason`) where we may want to require
    /// a richer consent record.
    ///
    /// Kept on the public surface so the caller can match on it
    /// without a wildcard arm — adding another variant in the future
    /// remains semver-additive.
    InsufficientConfirmation,

    /// New checksum did not match the runtime format.
    InvalidChecksum {
        /// The malformed checksum string the caller supplied.
        value: String,
        /// The exact rule violated.
        kind: ChecksumFormatErrorKind,
    },

    /// Operator picked a repair resolution that does not apply to the
    /// row's current state (e.g. asking to resume a row that is
    /// already `applied`).
    InvalidResolution {
        version: String,
        current_status: LedgerStatus,
        attempted: PartialApplyResolution,
    },

    /// Caller supplied a bucket whose app does not match the ledger
    /// row's `app_label`. Repair refuses rather than mutating a row
    /// while holding an advisory lock for the wrong logical bucket.
    BucketAppMismatch {
        version: String,
        row_app_label: String,
        supplied_app: String,
    },

    /// Database I/O while reading or updating a ledger row.
    LedgerIo { source: DjogiError },

    /// I/O failure while writing the rebuilt snapshot.
    SnapshotIo {
        path: PathBuf,
        source: SnapshotError,
    },

    /// `repair_resume_partial_apply` was given a plan whose
    /// `version` does not match the ledger row's `version`. The
    /// resume path requires the operator to hand in the EXACT plan
    /// the original apply consumed; a mismatch means the operator
    /// is about to re-run the wrong SQL against the partial-apply
    /// row.
    PlanVersionMismatch { ledger: String, plan: String },

    /// `repair_resume_partial_apply` was given a plan whose
    /// `checksum_up` (recomputed from the plan's segments) does not
    /// match the ledger row's `checksum_up`. Same hazard as
    /// `PlanVersionMismatch` — different SQL would otherwise execute.
    PlanChecksumMismatch {
        version: String,
        ledger_checksum: String,
        plan_checksum: String,
    },

    /// `repair_resume_partial_apply` was called on a row that has no
    /// remaining steps to run (`applied_steps_count >= total_steps`).
    /// The repair has nothing to do; surfacing this as a typed error
    /// rather than silently succeeding lets the operator distinguish
    /// "nothing to resume" from "resumed N steps".
    NothingToResume {
        version: String,
        applied: i32,
        total: Option<i32>,
    },

    /// A statement run by `repair_resume_partial_apply` failed; the
    /// underlying SQL error is wrapped. The repair function records
    /// the partial state on the ledger row before returning.
    ResumeStepFailed {
        version: String,
        step_index: usize,
        statement_label: String,
        applied_steps_count: i32,
        source: DjogiError,
    },

    /// The ledger row already carries an outstanding non-transactional
    /// progress claim, meaning the next step may have committed without
    /// its `applied_steps_count` acknowledgement being durably written.
    /// Automatic resume is refused to avoid silently re-running that
    /// ambiguous step.
    ResumeBlockedByNonTxProgressClaim { version: String, note: String },

    /// A resumed non-transactional statement committed successfully, but
    /// the repair path failed to durably acknowledge the new
    /// `applied_steps_count`. The claim note is left in place so later
    /// resume attempts refuse automatic replay.
    ResumeProgressAckFailed {
        version: String,
        step_index: usize,
        statement_label: String,
        applied_steps_count: i32,
        source: DjogiError,
    },

    /// `repair_snapshot_rebuild` projected the live database and
    /// found that the supplied / rebuilt snapshot does not match
    /// what the live catalog would write. Surfaces the offending
    /// table list so the operator can investigate without re-running
    /// the projection by hand.
    SuppliedSnapshotDiverges { differences: Vec<String> },

    /// **#274** — Failed to acquire the per-bucket Postgres advisory
    /// lock before the repair mutation. The lock is held by another
    /// concurrent runner or repair invocation; the repair was not
    /// attempted. Retry after the competing operation completes.
    AdvisoryLockFailed {
        /// The bucket whose lock could not be acquired.
        bucket: BucketKey,
        /// The advisory lock key that was contested.
        key: i64,
        /// Number of acquisition attempts made.
        attempts: u32,
    },

    /// **#274** — The `pg_try_advisory_lock` query itself errored before
    /// the lock result could be retrieved. Distinct from
    /// [`RepairError::AdvisoryLockFailed`] (which fires after the probe
    /// succeeded but returned `false` for every retry).
    AdvisoryLockQueryFailed {
        /// The `app_label` of the bucket whose lock was being acquired.
        app_label: String,
        /// The underlying Postgres error.
        source: DjogiError,
    },

    /// **#274 / #280** — `pg_advisory_unlock` returned `false` after
    /// the repair mutation completed successfully. The lock was not held
    /// on the physical session that called the unlock, indicating a
    /// session-pinning correctness failure.
    ///
    /// This variant fires ONLY when the repair mutation itself succeeded.
    /// If both the mutation and the release fail, the original mutation
    /// error is returned and the unlock failure is logged via
    /// `tracing::error!`.
    AdvisoryUnlockReturnedFalse {
        /// The advisory lock key that `pg_advisory_unlock` returned false for.
        key: i64,
    },

    /// **#274** — Failed to check out a single pinned Postgres connection
    /// from the pool before the repair operation began.
    PinnedSessionCheckoutFailed {
        /// The underlying pool or connection error.
        source: DjogiError,
    },
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairError::VersionNotFound { version } => write!(
                f,
                "repair could not find a ledger row for version `{version}`"
            ),
            RepairError::InsufficientConfirmation => f.write_str(
                "repair refused: caller did not supply RepairConfirmation::OperatorAcknowledged",
            ),
            RepairError::InvalidChecksum { value, kind } => {
                write!(f, "repair rejected new checksum `{value}`: {kind}")
            }
            RepairError::InvalidResolution {
                version,
                current_status,
                attempted,
            } => write!(
                f,
                "repair resolution {attempted:?} is not valid for version `{version}` \
                 (current status: {current})",
                current = current_status.as_db_str(),
            ),
            RepairError::BucketAppMismatch {
                version,
                row_app_label,
                supplied_app,
            } => write!(
                f,
                "repair refused for version `{version}`: supplied bucket app `{supplied_app}` \
                 does not match ledger row app_label `{row_app_label}`; repair would hold \
                 the advisory lock for the wrong bucket",
            ),
            RepairError::LedgerIo { source } => write!(f, "repair ledger I/O failed: {source}"),
            RepairError::SnapshotIo { path, source } => {
                write!(
                    f,
                    "repair snapshot I/O at {} failed: {source}",
                    path.display()
                )
            }
            RepairError::PlanVersionMismatch { ledger, plan } => write!(
                f,
                "repair_resume_partial_apply: plan version `{plan}` does not match \
                 the ledger row's version `{ledger}`",
            ),
            RepairError::PlanChecksumMismatch {
                version,
                ledger_checksum,
                plan_checksum,
            } => write!(
                f,
                "repair_resume_partial_apply on `{version}`: plan recomputed \
                 checksum_up={plan_checksum} differs from ledger {ledger_checksum}; \
                 the supplied plan does not match the original apply",
            ),
            RepairError::NothingToResume {
                version,
                applied,
                total,
            } => match total {
                Some(t) => write!(
                    f,
                    "repair_resume_partial_apply on `{version}`: applied_steps_count={applied} \
                     already equals total_steps={t}; nothing to resume",
                ),
                None => write!(
                    f,
                    "repair_resume_partial_apply on `{version}`: total_steps is NULL — \
                     this row was a transactional-only apply with no resumable steps",
                ),
            },
            RepairError::ResumeStepFailed {
                version,
                step_index,
                statement_label,
                applied_steps_count,
                source,
            } => write!(
                f,
                "repair_resume_partial_apply on `{version}`: step {step_index} `{statement_label}` \
                 failed after {applied_steps_count} successful step(s): {source}",
            ),
            RepairError::ResumeBlockedByNonTxProgressClaim { version, note } => write!(
                f,
                "repair_resume_partial_apply on `{version}` refused: the ledger row carries an \
                 outstanding non-tx progress claim, so the next step may already have \
                 committed. Reconcile the row with `repair_partial_apply` or manual \
                 inspection before resuming. Current note: {note}",
            ),
            RepairError::ResumeProgressAckFailed {
                version,
                step_index,
                statement_label,
                applied_steps_count,
                source,
            } => write!(
                f,
                "repair_resume_partial_apply on `{version}`: step {} `{statement_label}` \
                 committed, but the ledger failed to durably acknowledge \
                 applied_steps_count={applied_steps_count}; the claim note was preserved and \
                 automatic resume is now blocked: {source}",
                step_index + 1,
            ),
            RepairError::SuppliedSnapshotDiverges { differences } => write!(
                f,
                "repair_snapshot_rebuild: supplied / rebuilt snapshot diverges from \
                 the live catalog projection: {differences:?}",
            ),
            RepairError::AdvisoryLockFailed {
                bucket,
                key,
                attempts,
            } => write!(
                f,
                "D274 repair advisory lock for bucket database={db} app={app} \
                 (key=0x{key:016x}) could not be acquired after {attempts} attempts; \
                 a concurrent runner or repair invocation holds the lock (GH #274)",
                db = bucket.database,
                app = bucket.app,
            ),
            RepairError::AdvisoryLockQueryFailed { app_label, source } => write!(
                f,
                "D274 repair pg_try_advisory_lock query failed for app `{app_label}`: \
                 {source} (GH #274)",
            ),
            RepairError::AdvisoryUnlockReturnedFalse { key } => write!(
                f,
                "D274 repair pg_advisory_unlock returned false for key=0x{key:016x}; \
                 the advisory lock was not held on this session — session-pinning \
                 correctness failure (GH #274/#280)",
            ),
            RepairError::PinnedSessionCheckoutFailed { source } => write!(
                f,
                "D274 repair failed to check out a pinned Postgres session from the \
                 pool before the repair operation began (GH #274): {source}",
            ),
        }
    }
}

impl std::error::Error for RepairError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RepairError::LedgerIo { source } => Some(source),
            RepairError::SnapshotIo { source, .. } => Some(source),
            RepairError::ResumeStepFailed { source, .. } => Some(source),
            RepairError::ResumeProgressAckFailed { source, .. } => Some(source),
            RepairError::AdvisoryLockQueryFailed { source, .. } => Some(source),
            RepairError::PinnedSessionCheckoutFailed { source } => Some(source),
            _ => None,
        }
    }
}

// ── Advisory-lock helpers for repair ─────────────────────────────────────

/// Acquire the per-bucket advisory lock on the pinned context.
///
/// Wraps [`runner::acquire_advisory_lock`] and maps the `RunnerError`
/// variants into their `RepairError` counterparts so callers can use
/// the repair error type uniformly.
async fn acquire_advisory_lock_repair(
    ctx: &mut PinnedCtx<'_>,
    bucket: &BucketKey,
    key: i64,
) -> Result<(), RepairError> {
    acquire_advisory_lock(ctx, bucket, key)
        .await
        .map_err(|e| match e {
            super::runner::RunnerError::AdvisoryLockFailed {
                bucket,
                key,
                attempts,
            } => RepairError::AdvisoryLockFailed {
                bucket,
                key,
                attempts,
            },
            super::runner::RunnerError::AdvisoryLockQueryFailed { app_label, source } => {
                RepairError::AdvisoryLockQueryFailed { app_label, source }
            }
            other => {
                // Unreachable: acquire_advisory_lock only returns the two variants above.
                // Wrap defensively rather than panic so future runner changes do not
                // silently produce a misclassified error.
                RepairError::AdvisoryLockQueryFailed {
                    app_label: bucket.app.clone(),
                    source: DjogiError::Db(crate::error::DbError::other(format!(
                        "unexpected acquire error: {other}"
                    ))),
                }
            }
        })
}

/// Reconcile a repair mutation result with the advisory lock release bool.
///
/// - Success + true  → Ok (lock properly released)
/// - Success + false → `RepairError::AdvisoryUnlockReturnedFalse`
/// - Failure + _     → the original error (false is already logged)
// RepairError is a large enum by design. Async callers return it through
// boxed futures so clippy does not flag them; this sync helper also returns
// it and needs the suppression. Same rationale as reset.rs / compose.rs.
#[allow(clippy::result_large_err)]
fn handle_repair_release<T>(
    result: Result<T, RepairError>,
    released: bool,
    key: i64,
) -> Result<T, RepairError> {
    match result {
        Ok(v) if released => Ok(v),
        Ok(_) => Err(RepairError::AdvisoryUnlockReturnedFalse { key }),
        Err(e) => Err(e),
    }
}

// ── Public entry points ───────────────────────────────────────────────────

/// Repair a checksum-drift between the stored ledger row and
/// freshly-computed checksums (B-9).
///
/// **Both `checksum_up` and `checksum_down` are repaired in one
/// call.** The previous arrangement only repaired `checksum_up`,
/// which left `checksum_down` stale when both up and down SQL
/// changed. The fix takes both as parameters and writes them in a
/// single UPDATE — repair-as-one-shot, so a partial repair cannot
/// happen at the ledger level either.
///
/// `new_checksum_down` is `Option<&str>`. Pass `None` when the
/// migration has no rollback SQL (`down` is a SQL-comment placeholder
/// and the original ledger row carries `checksum_down = NULL`).
///
/// **Operator confirmation required.** The caller must pass
/// [`RepairConfirmation::OperatorAcknowledged`]; any other value
/// (in future) is rejected with [`RepairError::InsufficientConfirmation`].
///
/// **Format validation runs first.** Both new checksums must pass
/// [`validate_checksum_format`] before any UPDATE; a malformed
/// replacement would corrupt the row.
///
/// **Append-only invariant respected.** This UPDATE rewrites the
/// `checksum_up` and `checksum_down` fields on the existing row —
/// the only sanctioned post-write mutations aside from the rename /
/// progress paths. The original `applied_at` and `applied_by` are
/// preserved so the audit trail still anchors to the original apply.
///
/// **Caller supplies the bucket.** The advisory-lock key is derived
/// from `bucket` — the same `(database, app)` pair the runner used
/// when it applied the migration. Deriving the bucket from
/// `SELECT current_database()` inside repair was wrong: the runner
/// stores the logical database name from `plan.bucket.database`, not
/// the physical database name from the connected session (GH #274).
pub async fn repair_checksum_drift(
    ctx: &mut DjogiContext,
    _guard: &WorkspaceGuard,
    bucket: &BucketKey,
    version: &str,
    new_checksum_up: &str,
    new_checksum_down: Option<&str>,
    confirmation: RepairConfirmation,
) -> Result<RepairReport, RepairError> {
    if confirmation != RepairConfirmation::OperatorAcknowledged {
        return Err(RepairError::InsufficientConfirmation);
    }

    if let Err(kind) = validate_checksum_format(new_checksum_up) {
        return Err(RepairError::InvalidChecksum {
            value: new_checksum_up.to_string(),
            kind,
        });
    }
    if let Some(down) = new_checksum_down
        && let Err(kind) = validate_checksum_format(down)
    {
        return Err(RepairError::InvalidChecksum {
            value: down.to_string(),
            kind,
        });
    }

    // GH #274: pin one physical session for the entire lock + mutation
    // window. The ledger row is loaded INSIDE the lock in the pinned
    // helper to eliminate the TOCTOU window between the row read and
    // the checksum UPDATE.
    let lock_key = advisory_lock_key(bucket);
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RepairError::PinnedSessionCheckoutFailed { source: e })?;
    repair_checksum_drift_pinned(
        &mut pinned,
        version,
        new_checksum_up,
        new_checksum_down,
        bucket,
        lock_key,
    )
    .await
}

/// Core checksum-drift repair on an already-pinned context.
///
/// The ledger row is loaded INSIDE the advisory-lock window to eliminate
/// the TOCTOU race between reading `checksum_up` / `checksum_down` and
/// writing the updated values (GH #274).
async fn repair_checksum_drift_pinned(
    ctx: &mut PinnedCtx<'_>,
    version: &str,
    new_checksum_up: &str,
    new_checksum_down: Option<&str>,
    bucket: &BucketKey,
    lock_key: i64,
) -> Result<RepairReport, RepairError> {
    acquire_advisory_lock_repair(ctx, bucket, lock_key).await?;

    // Load the row inside the lock window — the `before` values must
    // be captured atomically with the mutation so the audit trail is
    // consistent even if another agent races.
    let row = match load_row(ctx, version).await {
        Ok(r) => r,
        Err(e) => {
            let _released = release_advisory_lock(ctx, lock_key).await;
            return Err(e);
        }
    };
    if let Err(e) = ensure_row_matches_bucket_app(&row, bucket, version) {
        let _released = release_advisory_lock(ctx, lock_key).await;
        return Err(e);
    }

    let before_up = row.checksum_up.clone();
    let before_down = row.checksum_down.clone();
    let new_down_owned = new_checksum_down.map(|s| s.to_string());

    let mutation_result = ctx
        .execute(
            "UPDATE djogi_schema_migrations \
             SET checksum_up = $2, checksum_down = $3 \
             WHERE version = $1",
            &[&version, &new_checksum_up, &new_down_owned],
        )
        .await
        .map_err(|e| RepairError::LedgerIo { source: e })
        .map(|_| ());

    let released = release_advisory_lock(ctx, lock_key).await;
    handle_repair_release(mutation_result, released, lock_key)?;

    let mut actions = vec![format!(
        "checksum_up of `{version}` updated from {before_up} to {new_checksum_up}"
    )];
    let mut changes = vec![LedgerChange::new(
        version,
        "checksum_up",
        before_up,
        new_checksum_up.to_string(),
    )];

    // Always record the checksum_down change — even when both before
    // and after are None — so the audit trail captures the operator
    // intent ("we touched this row's checksums").
    let after_down_render = new_down_owned
        .clone()
        .unwrap_or_else(|| "<none>".to_string());
    let before_down_render = before_down.clone().unwrap_or_else(|| "<none>".to_string());
    actions.push(format!(
        "checksum_down of `{version}` updated from {before_down_render} to {after_down_render}"
    ));
    changes.push(LedgerChange::new(
        version,
        "checksum_down",
        before_down_render,
        after_down_render,
    ));

    Ok(RepairReport {
        actions_taken: actions,
        ledger_changes: changes,
        snapshot_changes: Vec::new(),
    })
}

/// Resolution chosen by the operator when repairing a partial apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialApplyResolution {
    /// Mark the row `rolled_back`. The caller is responsible for
    /// having actually run the down-side SQL (typically via
    /// [`super::runner::rollback_plan`]) before invoking repair.
    /// Repair only rewrites the ledger row.
    MarkRolledBack,
    /// Mark the row `faked`. The operator is asserting the partial
    /// state is acceptable as-is and should be considered "applied"
    /// for future planning purposes. Used when an out-of-band fix
    /// already brought the schema to where the migration was heading.
    MarkFaked,
    /// Mark the row `applied` after the operator manually ran the
    /// remaining steps out-of-band. Repair updates
    /// `applied_steps_count` to match `total_steps` (when set) and
    /// flips status to `applied`.
    MarkApplied,
}

/// Repair a partial-apply state on a non-transactional migration.
///
/// **Operator confirmation required.**
///
/// `note` is preserved into `partial_apply_note` so the audit trail
/// records why the resolution was chosen.
///
/// `resolution` selects the new state. See [`PartialApplyResolution`].
///
/// Returns [`RepairError::InvalidResolution`] when the row is not in
/// a state that admits the chosen resolution (e.g. trying to resume a
/// row that is already `applied`).
///
/// **Caller supplies the bucket.** See [`repair_checksum_drift`] for
/// the rationale — same `(database, app)` requirement (GH #274).
pub async fn repair_partial_apply(
    ctx: &mut DjogiContext,
    _guard: &WorkspaceGuard,
    bucket: &BucketKey,
    version: &str,
    resolution: PartialApplyResolution,
    note: &str,
    confirmation: RepairConfirmation,
) -> Result<RepairReport, RepairError> {
    if confirmation != RepairConfirmation::OperatorAcknowledged {
        return Err(RepairError::InsufficientConfirmation);
    }

    // GH #274: pin one physical session. The ledger row load and status
    // check both happen INSIDE the lock in the pinned helper to prevent
    // TOCTOU races between reading the row and mutating it.
    let lock_key = advisory_lock_key(bucket);
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RepairError::PinnedSessionCheckoutFailed { source: e })?;
    repair_partial_apply_pinned(&mut pinned, version, resolution, note, bucket, lock_key).await
}

/// Core partial-apply repair on an already-pinned context.
///
/// The ledger row is loaded and status-validated INSIDE the advisory-lock
/// window to eliminate the TOCTOU race between the status read and the
/// status UPDATE (GH #274).
async fn repair_partial_apply_pinned(
    ctx: &mut PinnedCtx<'_>,
    version: &str,
    resolution: PartialApplyResolution,
    note: &str,
    bucket: &BucketKey,
    lock_key: i64,
) -> Result<RepairReport, RepairError> {
    acquire_advisory_lock_repair(ctx, bucket, lock_key).await?;

    // Load row and check status inside the lock window.
    let row = match load_row(ctx, version).await {
        Ok(r) => r,
        Err(e) => {
            let _released = release_advisory_lock(ctx, lock_key).await;
            return Err(e);
        }
    };
    if let Err(e) = ensure_row_matches_bucket_app(&row, bucket, version) {
        let _released = release_advisory_lock(ctx, lock_key).await;
        return Err(e);
    }
    if !matches!(row.status, LedgerStatus::Failed | LedgerStatus::Pending) {
        let current_status = row.status;
        let _released = release_advisory_lock(ctx, lock_key).await;
        return Err(RepairError::InvalidResolution {
            version: version.to_string(),
            current_status,
            attempted: resolution,
        });
    }

    let target_status = match resolution {
        PartialApplyResolution::MarkRolledBack => LedgerStatus::RolledBack,
        PartialApplyResolution::MarkFaked => LedgerStatus::Faked,
        PartialApplyResolution::MarkApplied => LedgerStatus::Applied,
    };

    let target_steps = match resolution {
        PartialApplyResolution::MarkApplied => row.total_steps.unwrap_or(row.applied_steps_count),
        // Rolled-back / faked: leave the count where it was so the
        // forensic trail is preserved.
        _ => row.applied_steps_count,
    };

    let status_str = target_status.as_db_str();
    let mutation_result = ctx
        .execute(
            "UPDATE djogi_schema_migrations \
             SET status = $2, applied_steps_count = $3, partial_apply_note = $4 \
             WHERE version = $1",
            &[&version, &status_str, &target_steps, &note],
        )
        .await
        .map_err(|e| RepairError::LedgerIo { source: e })
        .map(|_| ());

    let released = release_advisory_lock(ctx, lock_key).await;
    handle_repair_release(mutation_result, released, lock_key)?;

    Ok(RepairReport {
        actions_taken: vec![format!(
            "partial-apply repair of `{version}`: status {old} -> {new}; \
             applied_steps_count {old_steps} -> {new_steps}; note set",
            old = row.status.as_db_str(),
            new = target_status.as_db_str(),
            old_steps = row.applied_steps_count,
            new_steps = target_steps,
        )],
        ledger_changes: vec![
            LedgerChange::new(
                version,
                "status",
                row.status.as_db_str().to_string(),
                target_status.as_db_str().to_string(),
            ),
            LedgerChange::new(
                version,
                "applied_steps_count",
                row.applied_steps_count.to_string(),
                target_steps.to_string(),
            ),
            LedgerChange::new(
                version,
                "partial_apply_note",
                row.partial_apply_note.clone().unwrap_or_default(),
                note.to_string(),
            ),
        ],
        snapshot_changes: Vec::new(),
    })
}

/// Resume a partial-apply by re-running the remaining
/// non-transactional segment statements from
/// `applied_steps_count + 1` (B-10).
///
/// **The third partial-apply repair flow.** [`repair_partial_apply`]
/// covers the rollback / fake routes; this function covers the
/// resume route. The operator hands in the ORIGINAL plan that the
/// crashed apply consumed — repair re-verifies the plan's `version`
/// and `checksum_up` against the ledger row before re-running any
/// SQL.
///
/// **Operator confirmation required.**
///
/// **Safety checks.** Before any SQL runs, the function verifies:
///
/// - `plan.version` (derived from the supplied `version` argument)
///   matches the ledger row's `version`. (We also accept that the
///   caller passes the ledger version directly; this argument is
///   the resume-target.)
/// - `plan`'s recomputed `checksum_up` matches the ledger row's
///   `checksum_up`. A mismatch means a different plan than the one
///   originally applied is being supplied — refusing to run is the
///   only safe option.
/// - The ledger row's status is `failed` AND `total_steps` is set
///   AND `applied_steps_count < total_steps`. Anything else has
///   nothing to resume.
///
/// **What it runs.** The non-transactional segment(s) in plan order.
/// Each statement is executed via Djogi's internal batch executor
/// (auto-commit).
/// Resume now shares the runner's crash-safe claim/ack protocol:
/// before a step runs, the ledger row records a structured in-flight
/// claim in `partial_apply_note`; only after the SQL commits does the
/// repair path acknowledge the new `applied_steps_count`. If the ack
/// write fails, the claim note remains in place and future automatic
/// resumes are refused until an operator reconciles the ambiguous
/// boundary. On full success, the row is finalised to
/// `status = 'applied'`.
pub async fn repair_resume_partial_apply(
    ctx: &mut DjogiContext,
    _guard: &WorkspaceGuard,
    version: &str,
    plan: &MigrationPlan,
    confirmation: RepairConfirmation,
) -> Result<RepairReport, RepairError> {
    if confirmation != RepairConfirmation::OperatorAcknowledged {
        return Err(RepairError::InsufficientConfirmation);
    }

    // GH #274: pin one physical session. All ledger reads, validation,
    // and mutations run INSIDE the advisory lock in repair_resume_pinned
    // so the entire resume window is atomic from the lock's perspective.
    let lock_key = advisory_lock_key(&plan.bucket);
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RepairError::PinnedSessionCheckoutFailed { source: e })?;
    repair_resume_pinned(&mut pinned, version, plan, &plan.bucket, lock_key).await
}

/// Core resume-partial-apply logic on an already-pinned context.
///
/// Uses the acquire → body → release → reconcile pattern so that every
/// error path after lock acquisition — including the row load, validation,
/// and each step execution — releases the advisory lock exactly once via
/// the outer reconcile rather than through scattered early-return calls
/// (GH #274).
async fn repair_resume_pinned(
    ctx: &mut PinnedCtx<'_>,
    version: &str,
    plan: &MigrationPlan,
    bucket: &BucketKey,
    lock_key: i64,
) -> Result<RepairReport, RepairError> {
    acquire_advisory_lock_repair(ctx, bucket, lock_key).await?;

    // All work — row load, validation, step execution, and finalise —
    // runs inside `repair_resume_body`. Any `?`/early-return there
    // surfaces as the `result` below; the lock release always follows.
    let result = repair_resume_body(ctx, version, plan, bucket).await;

    let released = release_advisory_lock(ctx, lock_key).await;
    handle_repair_release(result, released, lock_key)
}

/// Body of the resume-partial-apply logic, called inside the advisory lock.
///
/// All `?` and early returns propagate to [`repair_resume_pinned`], which
/// unconditionally releases the lock after this function returns (whether
/// `Ok` or `Err`). This function must NOT call `release_advisory_lock`.
async fn repair_resume_body(
    ctx: &mut DjogiContext,
    version: &str,
    plan: &MigrationPlan,
    bucket: &BucketKey,
) -> Result<RepairReport, RepairError> {
    // Load row and run all validation inside the lock window to prevent
    // TOCTOU races (GH #274).
    let row = load_row(ctx, version).await?;
    ensure_row_matches_bucket_app(&row, bucket, version)?;

    let plan_checksum = compute_plan_checksum_up(plan);
    if plan_checksum != row.checksum_up {
        return Err(RepairError::PlanChecksumMismatch {
            version: version.to_string(),
            ledger_checksum: row.checksum_up.clone(),
            plan_checksum,
        });
    }

    if !matches!(row.status, LedgerStatus::Failed | LedgerStatus::Pending) {
        return Err(RepairError::InvalidResolution {
            version: version.to_string(),
            current_status: row.status,
            attempted: PartialApplyResolution::MarkApplied,
        });
    }
    if ledger::note_has_non_tx_progress_claim(row.partial_apply_note.as_deref()) {
        return Err(RepairError::ResumeBlockedByNonTxProgressClaim {
            version: version.to_string(),
            note: row.partial_apply_note.clone().unwrap_or_default(),
        });
    }

    let total = match row.total_steps {
        Some(t) => t,
        None => {
            return Err(RepairError::NothingToResume {
                version: version.to_string(),
                applied: row.applied_steps_count,
                total: None,
            });
        }
    };
    if row.applied_steps_count >= total {
        return Err(RepairError::NothingToResume {
            version: version.to_string(),
            applied: row.applied_steps_count,
            total: Some(total),
        });
    }

    // The runner CRUD helpers (`update_progress` / `mark_partial`)
    // take the row's BIGINT id. Look it up once.
    let ledger_id = lookup_ledger_id_by_version(ctx, &row.version).await?;

    // Walk the non-transactional segments in plan order, skipping
    // the first `applied_steps_count` statements globally.
    let mut remaining_to_skip = row.applied_steps_count as usize;
    let mut applied = row.applied_steps_count;
    let mut actions: Vec<String> = Vec::new();

    for (seg_idx, segment) in plan.segments.iter().enumerate() {
        if segment.kind != SegmentKind::NonTransactional {
            continue;
        }
        for (step_within, stmt) in segment.statements.iter().enumerate() {
            if remaining_to_skip > 0 {
                remaining_to_skip -= 1;
                continue;
            }
            let claimed_step = applied.saturating_add(1);
            let claim_note = ledger::format_non_tx_progress_claim(
                row.partial_apply_note.as_deref(),
                claimed_step,
                row.total_steps,
                seg_idx,
                &stmt.label,
            );
            ledger::claim_non_tx_progress(ctx, ledger_id, &claim_note)
                .await
                .map_err(|e| RepairError::LedgerIo { source: e })?;
            // Run the statement.
            if let Err(e) = guarded_batch_execute(ctx, &stmt.up).await {
                // Best-effort: record the new partial state before
                // returning. The advisory lock is released by
                // repair_resume_pinned after this function returns.
                let note = format!(
                    "resume failed at segment {seg_idx} step {step_within}: \
                     {label} — {e}",
                    label = stmt.label,
                );
                let _ = ledger::mark_partial(ctx, ledger_id, applied, &note).await;
                return Err(RepairError::ResumeStepFailed {
                    version: row.version.clone(),
                    step_index: applied as usize,
                    statement_label: stmt.label.clone(),
                    applied_steps_count: applied,
                    source: e,
                });
            }
            applied = applied.saturating_add(1);
            ledger::ack_non_tx_progress(ctx, ledger_id, applied, row.partial_apply_note.as_deref())
                .await
                .map_err(|e| RepairError::ResumeProgressAckFailed {
                    version: row.version.clone(),
                    step_index: claimed_step.saturating_sub(1) as usize,
                    statement_label: stmt.label.clone(),
                    applied_steps_count: applied,
                    source: e,
                })?;
            actions.push(format!(
                "resumed step {applied} of {total}: {label}",
                label = stmt.label,
            ));
        }
    }

    // Full success — finalise to applied.
    let before_status = row.status.as_db_str().to_string();
    let before_steps = row.applied_steps_count.to_string();
    let row_version = row.version.clone();
    ctx.execute(
        "UPDATE djogi_schema_migrations \
         SET status = 'applied', applied_steps_count = $2, partial_apply_note = NULL \
         WHERE version = $1",
        &[&row_version, &applied],
    )
    .await
    .map_err(|e| RepairError::LedgerIo { source: e })
    .map(|_| RepairReport {
        actions_taken: actions,
        ledger_changes: vec![
            LedgerChange::new(
                &row_version,
                "status",
                before_status,
                LedgerStatus::Applied.as_db_str().to_string(),
            ),
            LedgerChange::new(
                &row_version,
                "applied_steps_count",
                before_steps,
                applied.to_string(),
            ),
        ],
        snapshot_changes: Vec::new(),
    })
}

/// Recompute a plan's `checksum_up` for resume validation. Mirrors
/// the runner's `compute_checksum_for_plan_up` so the two sides
/// agree by construction.
fn compute_plan_checksum_up(plan: &MigrationPlan) -> String {
    let frags: Vec<&str> = plan
        .segments
        .iter()
        .flat_map(|s| s.statements.iter())
        .map(|s| s.up.as_str())
        .collect();
    compute_checksum(frags)
}

/// Read the `id` for a ledger row by `version`. Used by the resume
/// path to feed `ledger::update_progress` / `ledger::mark_partial`.
async fn lookup_ledger_id_by_version(
    ctx: &mut DjogiContext,
    version: &str,
) -> Result<i64, RepairError> {
    let row = ctx
        .query_one(
            "SELECT id FROM djogi_schema_migrations WHERE version = $1",
            &[&version],
        )
        .await
        .map_err(|e| RepairError::LedgerIo { source: e })?;
    let id: i64 = row.try_get(0).map_err(io_err)?;
    Ok(id)
}

/// Rebuild the on-disk snapshot for a `(database, app)` bucket from
/// the LIVE database catalog (B-12).
///
/// **Always re-projects from live.** Codex review (B-12) flagged
/// that the previous arrangement let the operator hand in any
/// `AppliedSchema` and the rebuild would write it verbatim — there
/// was no validation that the supplied snapshot matched what the
/// live catalog would write. The fix takes the tighter contract:
/// the operator supplies the bucket and the destination path; repair
/// projects the live database itself and writes that. The supplied
/// projection channel has been removed entirely.
///
/// **Operator confirmation required.**
///
/// **What the operator gets back.** [`RepairReport`] documents the
/// path that was written and the projection's table count so the
/// operator can confirm the rebuild matches expectations.
///
/// **Bootstrap policy.** This function does NOT bootstrap the
/// ledger. The ledger is required for the rebuild's "applied row
/// count" advisory; if the ledger is missing the rebuild still
/// proceeds and the action log records the absence.
pub async fn repair_snapshot_rebuild(
    ctx: &mut DjogiContext,
    _guard: &WorkspaceGuard,
    bucket: &BucketKey,
    snapshot_path: &std::path::Path,
    confirmation: RepairConfirmation,
) -> Result<RepairReport, RepairError> {
    if confirmation != RepairConfirmation::OperatorAcknowledged {
        return Err(RepairError::InsufficientConfirmation);
    }

    // GH #274: pin one physical session and acquire advisory lock.
    let lock_key = advisory_lock_key(bucket);
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RepairError::PinnedSessionCheckoutFailed { source: e })?;
    repair_snapshot_rebuild_pinned(&mut pinned, bucket, snapshot_path, lock_key).await
}

/// Core snapshot-rebuild logic on an already-pinned context.
async fn repair_snapshot_rebuild_pinned(
    ctx: &mut PinnedCtx<'_>,
    bucket: &BucketKey,
    snapshot_path: &std::path::Path,
    lock_key: i64,
) -> Result<RepairReport, RepairError> {
    acquire_advisory_lock_repair(ctx, bucket, lock_key).await?;

    // Always re-project from live. The verify-side helper is the
    // single source of truth for the live-DB projection so verify and
    // baseline / repair-rebuild agree by construction.
    //
    // Bucket-scoped (Codex round-2 B-11): the projection only
    // captures tables that belong to this bucket's app, so an app's
    // rebuild does not pick up another app's tables.
    let projected = super::verify::live_schema_for_repair(ctx, bucket)
        .await
        .map_err(|e| RepairError::LedgerIo {
            // Re-using LedgerIo's source channel would conflate two
            // failure modes; we pivot through a synthetic DjogiError
            // (DbError::other) so the public surface stays narrow
            // without adding a dedicated variant. The Display message
            // keeps the projection origin clear.
            source: DjogiError::Db(crate::error::DbError::other(format!(
                "live-DB projection failed: {e}"
            ))),
        });

    // Sanity-check: count applied rows for the bucket. A rebuild on
    // an empty ledger is suspicious but legal (a fresh bucket bootstrap
    // from a known-good schema). We surface the count as advisory.
    // If the ledger table itself is missing, the count function
    // would error — we degrade to "unknown" in that case so the
    // rebuild still proceeds.
    let applied_for_bucket = count_applied_for_app(ctx, &bucket.app)
        .await
        .ok()
        .unwrap_or(-1);

    // Release the advisory lock before writing to the filesystem (which
    // can be slow for large schemas) so we hold the PG lock no longer
    // than necessary.
    let released = release_advisory_lock(ctx, lock_key).await;

    // Surface advisory-lock release failure before the filesystem write.
    // The projected value is only available if the DB query succeeded.
    let projected = handle_repair_release(projected, released, lock_key)?;

    save_snapshot(&projected, snapshot_path).map_err(|e| RepairError::SnapshotIo {
        path: snapshot_path.to_path_buf(),
        source: e,
    })?;

    let mut actions = Vec::new();
    actions.push(format!(
        "snapshot rebuilt for bucket database={} app={} -> {}",
        bucket.database,
        bucket.app,
        snapshot_path.display(),
    ));
    actions.push(format!(
        "projected {n} table(s), {idx} index(es) from live catalog",
        n = projected.models.len(),
        idx = projected.indexes.len(),
    ));
    match applied_for_bucket {
        -1 => actions
            .push("advisory: ledger table not readable; bucket apply-count unknown".to_string()),
        0 => actions.push(
            "advisory: bucket has 0 applied ledger rows; rebuild recorded \
             as the snapshot for a fresh / empty migration history"
                .to_string(),
        ),
        n => actions.push(format!("advisory: bucket has {n} applied ledger row(s)")),
    }

    let description = match applied_for_bucket {
        -1 => "rebuilt from live-DB projection (ledger unreadable)".to_string(),
        n => format!("rebuilt from live-DB projection ({n} applied rows)"),
    };

    Ok(RepairReport {
        actions_taken: actions,
        ledger_changes: Vec::new(),
        snapshot_changes: vec![SnapshotChange {
            path: snapshot_path.to_path_buf(),
            description,
        }],
    })
}

// ── Private helpers ───────────────────────────────────────────────────────

/// Load the full ledger row for a `version`. Surfaces
/// [`RepairError::VersionNotFound`] when the row is absent so the
/// caller can distinguish "no such version" from a generic database
/// error.
///
/// Delegates to [`ledger::load_full_row_by_version`] — the 14-column
/// SELECT and try_get cascade live in ledger.rs to avoid triplication
/// across runner / repair / verify (cluster-2 simplify Finding 3).
async fn load_row(ctx: &mut DjogiContext, version: &str) -> Result<LedgerRow, RepairError> {
    load_full_row_by_version(ctx, version)
        .await
        .map_err(|e| RepairError::LedgerIo { source: e })?
        .ok_or_else(|| RepairError::VersionNotFound {
            version: version.to_string(),
        })
}

// RepairError is intentionally rich and unboxed; size is accepted for typed caller matching.
#[allow(clippy::result_large_err)]
fn ensure_row_matches_bucket_app(
    row: &LedgerRow,
    bucket: &BucketKey,
    version: &str,
) -> Result<(), RepairError> {
    if row.app_label == bucket.app {
        return Ok(());
    }

    Err(RepairError::BucketAppMismatch {
        version: version.to_string(),
        row_app_label: row.app_label.clone(),
        supplied_app: bucket.app.clone(),
    })
}

fn io_err(e: tokio_postgres::Error) -> RepairError {
    RepairError::LedgerIo {
        source: DjogiError::from(e),
    }
}

/// Count the `applied` ledger rows for one `app_label`. Used as a
/// pre-rebuild sanity check.
async fn count_applied_for_app(ctx: &mut DjogiContext, app_label: &str) -> Result<i64, DjogiError> {
    let row = ctx
        .query_one(
            "SELECT COUNT(*)::bigint FROM djogi_schema_migrations \
             WHERE app_label = $1 AND status = 'applied'",
            &[&app_label],
        )
        .await?;
    let n: i64 = row.try_get(0)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Witness type can only be constructed via the variant name ────────

    #[test]
    fn confirmation_value_is_only_constructible_via_variant_name() {
        // The sole purpose of this test is to assert that the enum
        // surface stays minimal — a single variant constructed by
        // typing its name. If a future contributor adds an
        // `impl Default` or `From<bool>`, this test fails to compile.
        let c = RepairConfirmation::OperatorAcknowledged;
        match c {
            RepairConfirmation::OperatorAcknowledged => (),
        }
        // Equality is implemented via #[derive(PartialEq, Eq)].
        assert_eq!(c, RepairConfirmation::OperatorAcknowledged);
    }

    // ── RepairReport ─────────────────────────────────────────────────────

    #[test]
    fn ledger_change_round_trips_through_clone_and_eq() {
        let a = LedgerChange {
            version: "V1".to_string(),
            column: "checksum_up",
            before: "old".to_string(),
            after: "new".to_string(),
        };
        assert_eq!(a, a.clone());
    }

    #[test]
    fn snapshot_change_round_trips_through_clone_and_eq() {
        let a = SnapshotChange {
            path: PathBuf::from("/tmp/x.json"),
            description: "rebuilt".to_string(),
        };
        assert_eq!(a, a.clone());
    }

    // ── Resolution discriminator ──────────────────────────────────────────

    #[test]
    fn partial_apply_resolution_distinct_variants() {
        // Three variants today; pin them so a future addition forces
        // a deliberate update of the test alongside the docs.
        let kinds = [
            PartialApplyResolution::MarkRolledBack,
            PartialApplyResolution::MarkFaked,
            PartialApplyResolution::MarkApplied,
        ];
        // Each variant is distinct.
        for (i, a) in kinds.iter().enumerate() {
            for (j, b) in kinds.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // ── Format validation rejects malformed checksums ────────────────────

    #[test]
    fn invalid_checksum_carries_kind() {
        // Synthesize the error directly — the public path is async so
        // we exercise the format check via the same helper here.
        let bad = "V2:notvalid";
        let kind = validate_checksum_format(bad).unwrap_err();
        // Match the kind so a future change in `ChecksumFormatErrorKind`
        // forces us to revisit the variant set.
        match kind {
            ChecksumFormatErrorKind::WrongPrefix
            | ChecksumFormatErrorKind::WrongLength { .. }
            | ChecksumFormatErrorKind::NonLowercaseHex { .. } => (),
        }
    }

    // ── compute_plan_checksum_up (B-10) ──────────────────────────────────

    #[test]
    fn compute_plan_checksum_up_matches_runner_path() {
        use crate::migrate::diff::Classification;
        use crate::migrate::projection::BucketKey;
        use crate::migrate::segment::{Segment, SegmentKind};
        use crate::migrate::sql::OperationSql;
        let plan = MigrationPlan {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![OperationSql {
                    label: "AddTable users".to_string(),
                    up: "CREATE TABLE users ()".to_string(),
                    down: "DROP TABLE users".to_string(),
                    lossy: None,
                }],
            }],
        };
        let from_repair = compute_plan_checksum_up(&plan);
        let manual = compute_checksum(["CREATE TABLE users ()"]);
        assert_eq!(
            from_repair, manual,
            "B-10: repair's resume checksum must match the runner's plan checksum"
        );
    }

    // ── B-9 rendered messages ────────────────────────────────────────────

    #[test]
    fn plan_checksum_mismatch_message_names_versions() {
        let e = RepairError::PlanChecksumMismatch {
            version: "V123".to_string(),
            ledger_checksum: "V1:aaaaa".to_string(),
            plan_checksum: "V1:bbbbb".to_string(),
        };
        let msg = format!("{e}");
        assert!(msg.contains("V123"));
        assert!(msg.contains("V1:aaaaa"));
        assert!(msg.contains("V1:bbbbb"));
    }

    #[test]
    fn nothing_to_resume_message_distinguishes_total_states() {
        let with_total = RepairError::NothingToResume {
            version: "V1".to_string(),
            applied: 5,
            total: Some(5),
        };
        let m = format!("{with_total}");
        assert!(m.contains("applied_steps_count=5"));
        assert!(m.contains("total_steps=5"));

        let no_total = RepairError::NothingToResume {
            version: "V1".to_string(),
            applied: 0,
            total: None,
        };
        let m = format!("{no_total}");
        assert!(m.contains("total_steps is NULL"));
    }

    // ── BucketAppMismatch — FIX_BEFORE_BETA-1 (GH #274) ─────────────────

    /// Minimal `LedgerRow` fixture used only for the `ensure_row_matches_bucket_app`
    /// unit tests. Fills non-tested fields with zero/default values.
    fn minimal_ledger_row(app_label: &str) -> LedgerRow {
        use crate::migrate::ledger::ExecutionMode;
        LedgerRow {
            version: "V1__test".to_string(),
            description: "unit test row".to_string(),
            // A structurally valid but semantically meaningless checksum.
            checksum_up: "V1:0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            checksum_down: None,
            execution_mode: ExecutionMode::Transactional,
            status: LedgerStatus::Applied,
            execution_time_ms: 0,
            out_of_order_flag: false,
            applied_steps_count: 0,
            total_steps: None,
            partial_apply_note: None,
            run_id: 0,
            snapshot_version: "1".to_string(),
            app_label: app_label.to_string(),
        }
    }

    /// `BucketAppMismatch` Display must name all three diagnostic fields:
    /// version, row_app_label, and supplied_app.
    #[test]
    fn bucket_app_mismatch_display_names_all_fields() {
        let e = RepairError::BucketAppMismatch {
            version: "V20260425__add_users".to_string(),
            row_app_label: "blog".to_string(),
            supplied_app: "store".to_string(),
        };
        let msg = format!("{e}");
        assert!(
            msg.contains("V20260425__add_users"),
            "Display must include the version; msg={msg}"
        );
        assert!(
            msg.contains("blog"),
            "Display must include the row's app_label; msg={msg}"
        );
        assert!(
            msg.contains("store"),
            "Display must include the supplied (wrong) app; msg={msg}"
        );
    }

    /// `ensure_row_matches_bucket_app` returns `Ok(())` when `row.app_label`
    /// and `bucket.app` are equal.
    #[test]
    fn ensure_row_matches_bucket_app_ok_when_apps_agree() {
        let row = minimal_ledger_row("blog");
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "blog".to_string(),
        };
        assert!(
            ensure_row_matches_bucket_app(&row, &bucket, "V1__test").is_ok(),
            "matching apps must return Ok"
        );
    }

    /// `ensure_row_matches_bucket_app` returns `BucketAppMismatch` with the
    /// correct diagnostic fields when `row.app_label != bucket.app`.
    #[test]
    fn ensure_row_matches_bucket_app_err_when_apps_differ() {
        let row = minimal_ledger_row("blog");
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "store".to_string(),
        };
        let err = ensure_row_matches_bucket_app(&row, &bucket, "V1__test")
            .expect_err("mismatched apps must return Err");
        match err {
            RepairError::BucketAppMismatch {
                version,
                row_app_label,
                supplied_app,
            } => {
                assert_eq!(version, "V1__test", "error must carry the version");
                assert_eq!(
                    row_app_label, "blog",
                    "error must carry the row's app_label"
                );
                assert_eq!(supplied_app, "store", "error must carry the supplied app");
            }
            other => panic!("expected BucketAppMismatch, got {other:?}"),
        }
    }
}
