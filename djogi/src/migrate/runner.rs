//! Migration runner — applies a [`MigrationPlan`] against a target
//! database, recording each step in the `djogi_schema_migrations`
//! ledger and persisting the snapshot only on full success.
//! # Lifecycle (/ §8)
//! ```text
//! 1. Acquire workspace file lock (guard primitive).
//! 2. Bootstrap djogi_schema_migrations table.
//! 3. Acquire pg_advisory_lock on a 64-bit key derived from BucketKey.
//! 4. Verify the supplied checksum matches a freshly-computed one.
//! 5. Insert the pending ledger row OUTSIDE the apply transaction.
//! 6. For each segment, dispatch by SegmentKind:
//! - Transactional → BEGIN; statements; COMMIT.
//! - NonTransactional → autocommit each statement; update progress.
//! - MetadataOnly → no SQL runs; metadata path is `compose`'s job.
//! 7. Phase 0 + `SingleNodeDev` only: provision node 1 after SQL and
//!    before terminal ledger status.
//! 8. On success: mark_applied + persist snapshot.
//! 9. On failure: mark_failed (or mark_partial for split-apply)
//! and propagate. Snapshot is NOT moved forward.
//! 10. Always: release pg_advisory_lock and workspace file lock.
//! ```
//! # Snapshot persistence invariant
//! The snapshot file at `migrations/<target>/<app>/schema_snapshot.json`
//! is written ONLY after the ledger row reaches `applied`. Any failure
//! transactional rollback, non-transactional crash, ledger update
//! error — leaves the snapshot at its prior value. This is the hard
//! invariant: if the snapshot
//! moved, every preceding migration succeeded.
//! # Determinism
//! Two runs against the same plan + same DB state must produce the
//! same ledger writes (modulo `applied_at`, `applied_by`,
//! `execution_time_ms`, and `run_id`). Hashing of `BucketKey` to the
//! advisory-lock key uses SHA-256 truncated to the low 64 bits — a
//! stable hash that does not depend on Rust's randomised default
//! `Hasher`.
//! # Relpages probe (.5)
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

use crate::__bypass::guarded_batch_execute;
use crate::config::{MigrateConfig, PolicyConfig};
use crate::context::{DjogiContext, PinnedCtx};
use crate::error::{DbError, DjogiError};
use crate::types::HeerId;

use super::guard::WorkspaceGuard;
use super::ledger::{
    self, ChecksumFormatError, ChecksumMismatch, ExecutionMode, LedgerRow, LedgerStatus,
    VerifyError, compute_checksum, load_full_row_by_version,
};
use super::projection::BucketKey;
use super::schema::SNAPSHOT_FORMAT_VERSION;
use super::segment::{MigrationPlan, Segment, SegmentKind};
use super::snapshot::{SnapshotError, save_snapshot};
use super::sql::{LossyRollbackKind, OperationSql};

// ── Public types ──────────────────────────────────────────────────────────

/// Why a migration statement is incompatible with the runner's
/// segment execution model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentSqlExecutionModeProblem {
    /// Top-level transaction control is forbidden in migration
    /// statements because the runner owns every BEGIN/COMMIT boundary.
    TransactionControl {
        /// Canonical leading keyword or keyword pair, e.g. `BEGIN`,
        /// `START TRANSACTION`, `RELEASE SAVEPOINT`.
        keyword: &'static str,
    },
    /// The statement must run outside any transaction, but the plan
    /// placed it in a transactional segment.
    RequiresNonTransactional {
        /// Canonical SQL shape that triggered the classification.
        statement_shape: &'static str,
    },
}

/// Errors surfaced by the runner. Each variant carries enough context
/// for an actionable operator message — no panicking, no silent
/// drops.
#[derive(Debug)]
#[non_exhaustive]
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

    /// The `pg_try_advisory_lock` probe itself failed (the query
    /// errored, or its boolean result could not be extracted).
    /// Distinct from [`RunnerError::AdvisoryLockFailed`] (which
    /// fires after the probe succeeded but returned `false` for
    /// every retry) and from [`RunnerError::LedgerWriteFailed`]
    /// (which is reserved for actual ledger writes — see
    /// cluster-2 Finding 6).
    AdvisoryLockQueryFailed {
        /// `app_label` of the bucket whose lock was being acquired.
        app_label: String,
        /// The underlying Postgres error.
        source: DjogiError,
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

    /// A read-only ledger query failed (SELECT against
    /// `djogi_schema_migrations` — out-of-order conflict probe,
    /// rollback row-fetch, etc.). Distinct from
    /// [`RunnerError::LedgerWriteFailed`]
    /// no row was written; the failure is in a reading probe and
    /// surfaces with a `query_label` so operators can correlate the
    /// error to the specific ledger probe (sibling of
    /// [`RunnerError::CatalogQueryFailed`] for the ledger surface).
    LedgerQueryFailed {
        /// A static label naming the ledger probe that failed (e.g.
        /// `"out_of_order_check"`, `"load_row_for_version"`).
        query_label: &'static str,
        /// The underlying Postgres error.
        source: DjogiError,
    },

    /// `SELECT heerid_next()` failed during runner startup, before
    /// the migration could touch the ledger. The run_id is a
    /// per-invocation HeerId stamped into every ledger row written
    /// by this run; failure here means we cannot tag rows for crash
    /// recovery and must abort the run before it begins.
    RunIdGenerationFailed { source: DjogiError },

    /// Node identity binding on the pinned migration connection
    /// failed. For non-Phase-0 apply this occurs before run-id
    /// generation, user SQL, or ledger insertion. For Phase 0
    /// `SingleNodeDev`, binding is part of the post-SQL provisioning
    /// step covered by [`RunnerError::SingleNodeDevProvisioningFailed`].
    NodeIdentityBindingFailed {
        /// The node ID that was attempted.
        node_id: i32,
        /// Source error from the binding SQL execution.
        source: DjogiError,
    },

    /// Phase 0 SQL succeeded, but explicit `SingleNodeDev`
    /// provisioning of node 1 failed before `mark_applied`. The
    /// runner marks the already-inserted pending ledger row `failed`
    /// and does not persist a snapshot.
    SingleNodeDevProvisioningFailed {
        /// The node ID that was being provisioned.
        node_id: i32,
        /// Which provisioning sub-step failed.
        step: &'static str,
        /// Source error from the provisioning SQL execution.
        source: DjogiError,
    },

    /// `ledger::bootstrap` failed — the ledger table's
    /// `CREATE TABLE IF NOT EXISTS` DDL could not run. Distinct from
    /// [`RunnerError::LedgerWriteFailed`] (row CRUD: INSERT/UPDATE)
    /// bootstrap is DDL, not row-level work, and almost always
    /// signals a permissions or connection problem rather than a
    /// data conflict.
    LedgerBootstrapFailed { source: DjogiError },

    /// Insertion of the pending ledger row collided with an existing
    /// row carrying the same `version`. Surfaces a typed error rather
    /// than a raw `LedgerWriteFailed { source: 23505 }` so operators
    /// re-running an already-applied migration get an actionable
    /// message that names the prior `applied_at` timestamp.
    VersionAlreadyApplied {
        version: String,
        applied_at: Option<OffsetDateTime>,
    },

    /// Insertion of the pending ledger row collided on `version`, but
    /// the existing row is in a non-terminal lifecycle status. Surface
    /// the row's status and run_id so operators can inspect and repair
    /// the blocking run instead of being told the migration is already
    /// applied.
    VersionCollisionNonTerminal {
        version: String,
        status: LedgerStatus,
        run_id: i64,
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

    /// A statement's SQL shape conflicts with the runner-managed
    /// execution mode for its segment. This is a preflight refusal:
    /// no ledger row is inserted and no SQL runs.
    SegmentSqlExecutionModeConflict {
        segment_index: usize,
        segment_kind: SegmentKind,
        statement_label: String,
        problem: SegmentSqlExecutionModeProblem,
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

    /// A non-transactional statement committed successfully, but the
    /// runner failed to durably acknowledge the step boundary on the
    /// ledger. The row is left with a structured claim note so repair
    /// resume refuses to replay the ambiguous step automatically.
    NonTransactionalProgressAckFailed {
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

    /// `baseline_plan` was called with `runner_ctx.snapshot.is_some()`.
    /// Baseline derives the canonical snapshot from a fresh live-DB
    /// projection so the operator-supplied channel is rejected
    /// up-front to prevent stale-snapshot baselines from poisoning
    /// future diffs.
    BaselineSnapshotShouldNotBeProvided,

    /// Phase 0 up-side artifact was classified as unsafe before the
    /// runner pinned a database session or touched the ledger.
    StalePhaseZeroArtifact {
        /// The Phase 0 migration version being refused.
        version: String,
        /// Classifier reason: `seed-capable-runtime-only`,
        /// `seed-dml-not-runtime-current`, `generated-stale`, `ambiguous`,
        /// `incomplete`, or `missing`.
        refusal_reason: &'static str,
    },

    /// The candidate version applies out-of-order (it sorts before
    /// some already-applied row in the same `(database, app)` bucket)
    /// AND the runner's [`super::policy::OutOfOrderPolicy`] is
    /// `Reject`. Surfaces the conflicting peer so the operator can
    /// decide between rebasing the migration to a later timestamp,
    /// supplying an explicit override, or reordering the apply.
    /// Note: this error fires BEFORE the pending ledger row is
    /// inserted, so a `Reject` outcome leaves no trace in the
    /// database. The operator-facing message is the only artifact.
    OutOfOrderRejected {
        /// The candidate version that triggered the conflict.
        version: String,
        /// The already-applied peer whose `version` is lexically
        /// greater than `version`.
        conflicting_version: String,
        /// `applied_at` of the conflicting peer, when available. The
        /// formatted RFC 3339 string is used so the message is
        /// timezone-explicit.
        conflicting_applied_at: Option<String>,
    },

    /// The live-DB projection underpinning `baseline_plan` failed
    /// before the ledger row could be inserted. Distinct from
    /// `LedgerWriteFailed` so the operator-facing message names the
    /// projection step (not the ledger) as the failing phase.
    BaselineProjectionFailed {
        source: Box<super::verify::VerifyRunError>,
    },

    /// Apply-time drift pre-flight found error-severity schema drift
    /// against the recorded snapshot baseline for a previously-applied
    /// bucket.
    DriftDetected {
        bucket: BucketKey,
        report: super::verify::VerifyReport,
    },

    /// Apply-time drift pre-flight required the recorded snapshot
    /// baseline for a previously-applied bucket, but the caller
    /// reported it missing on disk.
    DriftBaselineMissing { bucket: BucketKey },

    /// The drift pre-flight baseline file exists but could not be read or
    /// deserialized. Same refusal contract as
    /// [`RunnerError::DriftBaselineMissing`]: no pending ledger row, no
    /// user-schema mutation, no migration SQL.
    DriftBaselineCorrupted { bucket: BucketKey, reason: String },

    /// Apply-time drift pre-flight could not complete its read-only
    /// verify projection before any migration SQL ran.
    DriftPreflightFailed {
        source: Box<super::verify::VerifyRunError>,
    },

    /// **D060** — PK-flip pre-flight: logical-replication apply
    /// machinery is active in this database. Walsenders surfaced via
    /// `pg_stat_replication` and/or local subscriptions surfaced via
    /// `pg_subscription` (`subenabled = true`) signal that a separate
    /// session may be applying replicated changes with
    /// `session_replication_role = 'replica'` — that mode suppresses
    /// the BEFORE row triggers the cutover relies on. Postgres does
    /// not let one backend introspect another backend's GUC settings
    /// from `pg_stat_activity`, so we surface the broader replication
    /// signal as the hazard. Operator action: pause the apply
    /// worker(s) (or `ALTER TABLE ... ENABLE ALWAYS TRIGGER zzz_*`)
    /// before retrying.
    PkFlipHazardReplicaSessions {
        /// Active walsenders observed via `pg_stat_replication`.
        /// `(application_name, client_addr_text)` pairs.
        walsenders: Vec<(String, String)>,
        /// Enabled local subscriptions observed via `pg_subscription`
        /// (subscribers run an apply worker in `replica` role). Each
        /// entry is the subscription name.
        subscriptions: Vec<String>,
    },

    /// **D061** — PK-flip pre-flight: a `zzz_*` trigger already
    /// exists on the migrating table (or one of its children).
    /// Collisions with the autofill trigger naming convention abort
    /// the install; the operator must rename or drop the existing
    /// trigger before retrying.
    PkFlipHazardPreexistingZzzTrigger {
        /// Postgres table carrying the offending trigger.
        table: String,
        /// Trigger names found.
        trigger_names: Vec<String>,
    },

    /// **D062** — PK-flip pre-flight: at least one trigger on the
    /// migrating table (or a child) is already disabled (`tgenabled
    /// <> 'O'`). A disabled trigger leaves writes during the window
    /// without their `_desc` shadow populated; the runner refuses.
    PkFlipHazardDisabledTriggers {
        /// Postgres table carrying the offending trigger.
        table: String,
        /// `(trigger_name, tgenabled_char)` pairs.
        triggers: Vec<(String, char)>,
    },

    /// **D063** — PK-flip pre-flight: at least one open Postgres
    /// transaction has run for longer than the configured threshold
    /// (`MigrateConfig::pk_flip_long_tx_threshold_secs`). The
    /// cutover would either block on `AccessExclusiveLock` or abort
    /// on `lock_timeout`; the runner refuses.
    PkFlipHazardLongRunningTx {
        /// `(pid, age_seconds)` pairs.
        offenders: Vec<(i32, i64)>,
        /// Threshold the offenders exceeded.
        threshold_secs: u32,
    },

    /// **D064** — PK-flip verification halt: between backfill and
    /// cutover, the per-table verification SELECT returned a non-zero
    /// count of NULL / mismatched shadow rows. The runner halts
    /// before opening the cutover transaction.
    PkFlipVerificationFailed {
        /// Postgres table whose verification failed.
        table: String,
        /// Number of violating rows the verification query returned.
        count_violating: i64,
    },

    /// A Postgres system-catalog query (pg_class, pg_constraint,
    /// pg_subscription, etc.) failed before the migration could
    /// proceed. Distinct from [`RunnerError::LedgerWriteFailed`]
    /// the ledger was not touched; the failure is in a read-only
    /// catalog probe. The `query_label` field names the probe so
    /// operators can correlate the error to the specific catalog
    /// object being queried (cluster-2 simplify Finding 6).
    CatalogQueryFailed {
        /// A static label naming the catalog object or query that
        /// failed (e.g. `"pg_stat_replication"`, `"pg_class relpages"`).
        query_label: &'static str,
        /// The underlying Postgres error.
        source: DjogiError,
    },

    /// **#274** — Failed to check out a single pinned Postgres connection
    /// from the pool before the migration operation began. The runner
    /// requires one physical session for the entire advisory-lock window
    /// (lock acquisition, DDL, ledger writes, and lock release must all
    /// occur on the same backend). Pool checkout failure means the
    /// operation cannot start; no ledger row is inserted and no DDL runs.
    PinnedSessionCheckoutFailed {
        /// The underlying pool or connection error.
        source: DjogiError,
    },

    /// **#274 / #280** — `pg_advisory_unlock` returned `false`, meaning
    /// the advisory lock was NOT held on the physical session that called
    /// it. This is a session-pinning correctness failure: the lock was
    /// either never acquired on this session or was acquired on a
    /// different one (the pre-#274 pool-backed bug).
    /// This variant fires ONLY when the migration operation itself
    /// succeeded. If both the operation and the release fail, the
    /// original operation error is returned and the unlock failure is
    /// logged via `tracing::error!`.
    AdvisoryUnlockReturnedFalse {
        /// The advisory lock key that `pg_advisory_unlock` returned false for.
        key: i64,
        /// The bucket whose advisory lock could not be confirmed as released.
        bucket: BucketKey,
    },

    /// Strict replay expansion refused: partitioned parent has zero leaves.
    /// In apply mode this falls back to a no-op comment; in replay-strict
    /// mode (rollback/repair) we refuse because replaying a shorter stream
    /// would silently miss leaf work that the ledger already counted.
    PartitionExpansionNoLeaves {
        parent: String,
        statement_label: String,
    },

    /// Non-Phase-0 apply/fake/baseline refused: the runner was called
    /// without a binding-capable [`RunnerIdentity`], so the process
    /// cannot bind node identity before `generate_run_id()` or user
    /// SQL execution. Phase 0 is exempt (uses wall-clock run-id).
    /// This gate mirrors the rollback inverse at [`rollback_handle_lock`]
    /// so the apply-family paths refuse missing identity like rollback
    /// already does.
    MissingApplyIdentity {
        /// The migration version that was refused.
        version: String,
    },
}

impl RunnerError {
    /// Classify a runner error for the CLI exit-code matrix.
    ///
    /// `true` means the error is an operator-actionable refusal (exit code `2`):
    /// a deterministic rejection with no meaningful retry path before intervention.
    /// `false` means the operation was attempted, failed, and can be retried by CI
    /// after a normal backoff / transient recovery (`1`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use djogi::migrate::{RunnerError, ledger::ChecksumMismatch};
    ///
    /// let refusal = RunnerError::ChecksumMismatch(ChecksumMismatch {
    ///     version: "V20260101000000__add_users".to_string(),
    ///     expected: "V1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
    ///     actual: "V1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
    /// });
    /// assert!(refusal.is_operator_actionable());
    ///
    /// let transient = RunnerError::LockTimeout {
    ///     path: std::path::PathBuf::from("/tmp/.djogi-migrations-lock"),
    ///     holder_pid: None,
    /// };
    /// assert!(!transient.is_operator_actionable());
    /// ```
    #[must_use]
    #[allow(clippy::match_like_matches_macro)]
    pub fn is_operator_actionable(&self) -> bool {
        match self {
            Self::ChecksumMismatch(_)
            | Self::ChecksumFormat(_)
            | Self::VersionAlreadyApplied { .. }
            | Self::VersionCollisionNonTerminal { .. }
            | Self::TargetTableNotFound { .. }
            | Self::RelpagesThresholdExceeded { .. }
            | Self::SegmentSqlExecutionModeConflict { .. }
            | Self::SnapshotPersistFailed { .. }
            | Self::BaselineSnapshotShouldNotBeProvided
            | Self::StalePhaseZeroArtifact { .. }
            | Self::DriftDetected { .. }
            | Self::DriftBaselineMissing { .. }
            | Self::DriftBaselineCorrupted { .. }
            | Self::OutOfOrderRejected { .. }
            | Self::PkFlipHazardReplicaSessions { .. }
            | Self::PkFlipHazardPreexistingZzzTrigger { .. }
            | Self::PkFlipHazardDisabledTriggers { .. }
            | Self::PkFlipHazardLongRunningTx { .. }
            | Self::PkFlipVerificationFailed { .. }
            | Self::AdvisoryUnlockReturnedFalse { .. }
            | Self::PartitionExpansionNoLeaves { .. }
            | Self::MissingApplyIdentity { .. } => true,

            Self::LockTimeout { .. }
            | Self::GuardError(_)
            | Self::AdvisoryLockFailed { .. }
            | Self::AdvisoryLockQueryFailed { .. }
            | Self::LedgerWriteFailed { .. }
            | Self::LedgerQueryFailed { .. }
            | Self::RunIdGenerationFailed { .. }
            | Self::NodeIdentityBindingFailed { .. }
            | Self::SingleNodeDevProvisioningFailed { .. }
            | Self::LedgerBootstrapFailed { .. }
            | Self::TransactionalSegmentFailed { .. }
            | Self::NonTransactionalSegmentFailed { .. }
            | Self::NonTransactionalProgressAckFailed { .. }
            | Self::ConfigLoadFailed { .. }
            | Self::BaselineProjectionFailed { .. }
            | Self::DriftPreflightFailed { .. }
            | Self::CatalogQueryFailed { .. }
            | Self::PinnedSessionCheckoutFailed { .. } => false,
        }
    }
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
            RunnerError::AdvisoryLockQueryFailed { app_label, source } => write!(
                f,
                "pg_try_advisory_lock query failed for app `{app_label}`: {source}",
            ),
            RunnerError::ChecksumMismatch(m) => write!(f, "{m}"),
            RunnerError::ChecksumFormat(e) => write!(f, "{e}"),
            RunnerError::LedgerWriteFailed { version, source } => {
                write!(f, "ledger write failed for version `{version}`: {source}")
            }
            RunnerError::LedgerQueryFailed {
                query_label,
                source,
            } => write!(
                f,
                "ledger query `{query_label}` failed before the migration could proceed: {source}",
            ),
            RunnerError::RunIdGenerationFailed { source } => write!(
                f,
                "run_id generation via `SELECT heerid_next()` failed before any \
                 migration ran: {source}",
            ),
            RunnerError::NodeIdentityBindingFailed { node_id, source } => write!(
                f,
                "runner node identity binding failed for node {node_id} \
                 (unregistered/inactive or SQL error): {source}",
            ),
            RunnerError::SingleNodeDevProvisioningFailed {
                node_id,
                step,
                source,
            } => write!(
                f,
                "single-node-dev provisioning failed for node {node_id} during {step}: {source}",
            ),
            RunnerError::LedgerBootstrapFailed { source } => write!(
                f,
                "ledger bootstrap (CREATE TABLE IF NOT EXISTS djogi_schema_migrations) \
                 failed: {source}",
            ),
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
            RunnerError::VersionCollisionNonTerminal {
                version,
                status,
                run_id,
            } => {
                let guidance = match status {
                    LedgerStatus::Pending => {
                        "use `djogi migrations status` to inspect it, then `repair_partial_apply` to resolve it in place"
                    }
                    LedgerStatus::Failed => {
                        "use `djogi migrations status` to inspect it, then `repair_resume_partial_apply` if it is still resumable or `repair_partial_apply` otherwise"
                    }
                    LedgerStatus::RolledBack => {
                        "use `djogi migrations status` to inspect it; the rolled_back row is retained as audit history, so use a new migration version or an explicit operator cleanup before re-applying"
                    }
                    LedgerStatus::Applied | LedgerStatus::Baseline | LedgerStatus::Faked => {
                        unreachable!(
                            "VersionCollisionNonTerminal only carries pending, failed, or rolled_back rows",
                        )
                    }
                };
                write!(
                    f,
                    "migration version `{version}` collided with an existing non-terminal \
                     ledger row (status `{status}`, run_id {run_id}); re-running is rejected \
                     until that run is reconciled — {guidance}",
                    status = status.as_db_str(),
                )
            }
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
            RunnerError::SegmentSqlExecutionModeConflict {
                segment_index,
                segment_kind,
                statement_label,
                problem,
            } => match problem {
                SegmentSqlExecutionModeProblem::TransactionControl { keyword } => write!(
                    f,
                    "{} segment {segment_index} statement `{statement_label}` embeds top-level \
                     transaction control `{keyword}`; djogi owns migration transaction boundaries \
                     and refuses inline BEGIN/COMMIT/SAVEPOINT control",
                    segment_kind_name(*segment_kind),
                ),
                SegmentSqlExecutionModeProblem::RequiresNonTransactional { statement_shape } => {
                    write!(
                        f,
                        "{} segment {segment_index} statement `{statement_label}` uses `{statement_shape}`, \
                     which must run in a non-transactional segment",
                        segment_kind_name(*segment_kind),
                    )
                }
            },
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
            RunnerError::NonTransactionalProgressAckFailed {
                segment_index,
                step_index,
                statement_label,
                applied_steps_count,
                source,
            } => write!(
                f,
                "non-transactional segment {segment_index} step {} `{statement_label}` \
                 committed, but the runner failed to durably acknowledge \
                 applied_steps_count={applied_steps_count}; the row now carries a \
                 non-tx progress claim and must be reconciled before resume: {source}",
                step_index + 1,
            ),
            RunnerError::ConfigLoadFailed { source } => {
                write!(f, "failed to load Djogi.toml: {source}")
            }
            RunnerError::SnapshotPersistFailed { path, source } => {
                write!(f, "snapshot persist failed at {}: {source}", path.display(),)
            }
            RunnerError::BaselineSnapshotShouldNotBeProvided => f.write_str(
                "baseline_plan rejects caller-supplied snapshots: baseline projects the \
                 live database itself; pass `runner_ctx.snapshot = None`",
            ),
            RunnerError::StalePhaseZeroArtifact {
                version,
                refusal_reason,
            } => write!(
                f,
                "Phase 0 artifact `{version}` refused before migration apply: {refusal_reason}",
            ),
            RunnerError::BaselineProjectionFailed { source } => write!(
                f,
                "baseline live-DB projection failed before ledger insert: {source}",
            ),
            RunnerError::DriftDetected { bucket, report } => {
                let errors = report
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity == super::verify::VerifySeverity::Error)
                    .count();
                write!(
                    f,
                    "drift pre-flight refused apply for database={db} app={app}: \
                     {errors} error-severity drift diagnostic(s) between the \
                     recorded snapshot and the live database; no migration SQL \
                     was executed and no pending ledger row was written. Inspect \
                     with `djogi migrations verify`; reconcile intentional drift \
                     with `djogi migrations attune`.",
                    db = bucket.database,
                    app = bucket.app,
                )
            }
            RunnerError::DriftBaselineMissing { bucket } => write!(
                f,
                "drift pre-flight refused apply for database={db} app={app}: \
                 the bucket has applied migration history but no recorded \
                 snapshot baseline (`schema_snapshot.json`) was found; no \
                 migration SQL was executed and no pending ledger row was \
                 written. Restore the snapshot from version control, or \
                 rebuild it from the live database with `djogi migrations \
                 repair snapshot-rebuild`.",
                db = bucket.database,
                app = bucket.app,
            ),
            RunnerError::DriftBaselineCorrupted { bucket, reason } => write!(
                f,
                "drift pre-flight refused apply for database={db} app={app}: \
                 the recorded snapshot baseline (`schema_snapshot.json`) exists \
                 but could not be read: {reason}. No migration SQL was executed \
                 and no pending ledger row was written. Restore the snapshot \
                 from version control or rebuild it with \
                 `djogi migrations repair snapshot-rebuild`.",
                db = bucket.database,
                app = bucket.app,
            ),
            RunnerError::DriftPreflightFailed { source } => {
                write!(f, "drift pre-flight could not run: {source}")
            }
            RunnerError::PkFlipHazardReplicaSessions {
                walsenders,
                subscriptions,
            } => write!(
                f,
                "D060 PK-flip cutover refused: logical-replication machinery is active and \
                 may be applying changes with session_replication_role = 'replica' (which \
                 suppresses BEFORE row triggers and would leave the autofill skipped). \
                 Pause the apply worker(s) or `ALTER TABLE ... ENABLE ALWAYS TRIGGER zzz_*` \
                 before retrying. Walsenders ({nw}): {walsenders:?}; \
                 enabled subscriptions ({ns}): {subscriptions:?}",
                nw = walsenders.len(),
                ns = subscriptions.len(),
            ),
            RunnerError::PkFlipHazardPreexistingZzzTrigger {
                table,
                trigger_names,
            } => write!(
                f,
                "D061 PK-flip cutover refused: pre-existing zzz_* trigger(s) on `{table}` \
                 collide with the autofill install: {trigger_names:?}. Rename or drop them \
                 before retrying.",
            ),
            RunnerError::PkFlipHazardDisabledTriggers { table, triggers } => write!(
                f,
                "D062 PK-flip cutover refused: disabled trigger(s) on `{table}` would \
                 silently bypass the autofill: {triggers:?}. Re-enable them or pause \
                 whatever process disabled them before retrying.",
            ),
            RunnerError::PkFlipHazardLongRunningTx {
                offenders,
                threshold_secs,
            } => write!(
                f,
                "D063 PK-flip cutover refused: {n} transaction(s) have been open longer \
                 than {threshold_secs}s and would block AccessExclusiveLock or trigger \
                 lock_timeout. Cancel or terminate them via pg_cancel_backend / \
                 pg_terminate_backend, then retry. Offenders (pid, age_secs): {offenders:?}",
                n = offenders.len(),
            ),
            RunnerError::PkFlipVerificationFailed {
                table,
                count_violating,
            } => write!(
                f,
                "D064 PK-flip verification halt: table `{table}` has {count_violating} row(s) \
                 with NULL or stale shadow values. Re-run the backfill (and audit any DISABLE \
                 TRIGGER / replica writes during the window) before retrying the cutover.",
            ),
            RunnerError::OutOfOrderRejected {
                version,
                conflicting_version,
                conflicting_applied_at,
            } => match conflicting_applied_at {
                Some(when) => write!(
                    f,
                    "version `{version}` would apply out-of-order: peer \
                     `{conflicting_version}` was already applied at {when}; \
                     the active OutOfOrderPolicy is Reject. Either rebase \
                     this migration to a later timestamp, supply \
                     OutOfOrderPolicy::AllowExplicit with an override reason, \
                     or run on a non-CI / non-production profile to inherit \
                     AllowWithDiagnostic."
                ),
                None => write!(
                    f,
                    "version `{version}` would apply out-of-order: peer \
                     `{conflicting_version}` is already applied; \
                     the active OutOfOrderPolicy is Reject."
                ),
            },
            RunnerError::CatalogQueryFailed {
                query_label,
                source,
            } => write!(f, "Postgres catalog query '{query_label}' failed: {source}",),
            RunnerError::PinnedSessionCheckoutFailed { source } => write!(
                f,
                "failed to check out a pinned Postgres session from the pool before \
                 the migration operation began (GH #274): {source}",
            ),
            // D274 is shared by runner and repair for advisory-lock
            // correctness failures (GH #274); shared code is intentional.
            RunnerError::AdvisoryUnlockReturnedFalse { key, bucket } => write!(
                f,
                "D274 pg_advisory_unlock returned false for bucket database={db} app={app} \
                 (key=0x{key:016x}); the advisory lock was not held on the session that \
                 called pg_advisory_unlock — this is a session-pinning correctness failure \
                 (GH #274/#280). The migration SQL and ledger writes may have succeeded; \
                 inspect the ledger row to determine the actual applied state.",
                db = bucket.database,
                app = bucket.app,
            ),
            RunnerError::PartitionExpansionNoLeaves {
                parent,
                statement_label,
            } => write!(
                f,
                "partition expansion for `{statement_label}` refused: \
                 partitioned parent `{parent}` has 0 leaves in replay-strict mode",
            ),
            RunnerError::MissingApplyIdentity { version } => write!(
                f,
                "apply refused: version '{version}' requires a binding-capable runner identity \
                 (not Phase 0, no runner_identity set)",
            ),
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
            RunnerError::AdvisoryLockQueryFailed { source, .. } => Some(source),
            RunnerError::SegmentSqlExecutionModeConflict { .. } => None,
            RunnerError::TransactionalSegmentFailed { source, .. } => Some(source),
            RunnerError::NonTransactionalSegmentFailed { source, .. } => Some(source),
            RunnerError::NonTransactionalProgressAckFailed { source, .. } => Some(source),
            RunnerError::ConfigLoadFailed { source } => Some(source),
            RunnerError::SnapshotPersistFailed { source, .. } => Some(source),
            RunnerError::StalePhaseZeroArtifact { .. } => None,
            RunnerError::BaselineProjectionFailed { source } => Some(source.as_ref()),
            RunnerError::DriftDetected { .. } => None,
            RunnerError::DriftBaselineMissing { .. } => None,
            RunnerError::DriftBaselineCorrupted { .. } => None,
            RunnerError::DriftPreflightFailed { source } => Some(source.as_ref()),
            RunnerError::CatalogQueryFailed { source, .. } => Some(source),
            RunnerError::LedgerQueryFailed { source, .. } => Some(source),
            RunnerError::RunIdGenerationFailed { source } => Some(source),
            RunnerError::NodeIdentityBindingFailed { source, .. } => Some(source),
            RunnerError::SingleNodeDevProvisioningFailed { source, .. } => Some(source),
            RunnerError::LedgerBootstrapFailed { source } => Some(source),
            RunnerError::PinnedSessionCheckoutFailed { source } => Some(source),
            RunnerError::VersionCollisionNonTerminal { .. } => None,
            _ => None,
        }
    }
}

/// Runner identity mode for identity-bearing migration execution.
///
/// The runner uses this to determine whether and how to bind node
/// identity on the pinned migration connection before non-Phase-0
/// `generate_run_id` / `HeerId::generate`, user SQL, and ledger
/// mutations.
///
/// **Three modes:**
/// 1. **`Selected { id }`** — Production/cluster mode with explicit
///    node ID selected by the operator (via CLI `--node-id` or env
///    `HEER_NODE_ID`). The runner binds this identity on the pinned
///    connection after Phase 0 installs HeeRanjID, and validates
///    active registration against `heer_nodes`.
/// 2. **`SingleNodeDev`** — Explicit dev/single-node mode (CLI
///    `--single-node-dev`). The runner provisions node 1 immediately
///    after Phase 0 SQL succeeds, then binds node 1 on the pinned
///    connection. Non-Phase-0 migrations only bind the already-
///    provisioned node.
/// 3. **`IdentityFree`** — Source-proven inverse path: no user
///    migration SQL and no non-Phase-0 `generate_run_id` /
///    `HeerId::generate`. The runner does not attempt any node
///    binding. Status, verify, attune record sentinel, squash
///    checksum refresh, repair checksum, repair partial status
///    updates, and snapshot rebuild fall here.
///
/// **Library contract.** The Djogi runtime library and pool must NOT
/// read `HEER_NODE_ID` or `HEER_RANJ_NODE_ID`. Identity resolution
/// is a CLI migration-runner resolver concern only. Library callers
/// pass explicit `RunnerIdentity` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerIdentity {
    /// Production/cluster: explicit node ID selected by the operator.
    Selected { id: i32 },
    /// Explicit dev/single-node: binds node 1.
    SingleNodeDev,
    /// Source-proven inverse: no user SQL and no non-Phase-0 run-id generation.
    IdentityFree,
}

impl RunnerIdentity {
    /// Return the concrete node ID for modes that carry one.
    /// `IdentityFree` returns `None`.
    pub(crate) fn node_id(self) -> Option<i32> {
        match self {
            Self::Selected { id } => Some(id),
            Self::SingleNodeDev => Some(1),
            Self::IdentityFree => None,
        }
    }

    /// Return `true` when this mode requires node binding on the
    /// pinned migration connection before non-Phase-0 run-id
    /// generation or user SQL.
    pub(crate) fn requires_binding(self) -> bool {
        !matches!(self, Self::IdentityFree)
    }

    /// Return `true` when Phase 0 should run post-SQL node-1
    /// provisioning for dev mode.
    pub(crate) fn provisions_phase_zero_single_node_dev(self) -> bool {
        matches!(self, Self::SingleNodeDev)
    }
}

/// How the apply-time drift pre-flight obtains its baseline.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DriftBaseline {
    /// Drift pre-flight is deliberately disabled by the caller.
    Disabled,
    /// The caller looked for the recorded snapshot and did not find it.
    Missing,
    /// The recorded snapshot baseline to verify against before apply.
    Snapshot(super::schema::AppliedSchema),
    /// The snapshot file exists but could not be deserialized (corrupt or
    /// wrong format). The runner skips the gate when the bucket has never
    /// been applied (drift is undefined without a prior applied state) and
    /// refuses with [`RunnerError::DriftBaselineCorrupted`] when the bucket
    /// has applied history.
    Corrupted(String),
}

/// Caller-supplied context for one runner invocation. Decouples the
/// runner from a hard-coded `Djogi.toml` location so tests can
/// inject their own knobs.
/// **Snapshot path policy.** `snapshot_path` is owned by the caller
/// the runner does not invent a path. The `apply` orchestrator
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
    /// Policy gate for out-of-order applies. Defaults to
    /// [`super::policy::OutOfOrderPolicy::AllowWithDiagnostic`] for
    /// dev iteration; CI / production loaders flip to `Reject` via
    /// [`super::policy::OutOfOrderPolicy::default_for_config`]. The
    /// runner consults this BEFORE inserting the pending ledger row,
    /// so a `Reject` outcome leaves no database trace.
    pub out_of_order_policy: super::policy::OutOfOrderPolicy,
    /// Optional pool pointing at the **audit DB** (`crud_log_url` in
    /// `Djogi.toml`).
    /// When `Some`, the runner writes one row to `djogi_ddl_audit`
    /// per successful migration via [`super::audit::record_ddl`],
    /// so `djogi db reset` (which drops the app DB) cannot erase
    /// the migration history. When `None` the audit write is
    /// silently skipped — appropriate for tests and for adopters
    /// who have not yet provisioned the second DB.
    /// **Wiring:**
    /// - Added the field and wired
    ///   [`super::record_ddl_audit`] into `apply_plan_inner`'s
    ///   success-only path. The runner writes audit rows whenever
    ///   the caller supplies `Some(pool)`.
    /// - **issue #118** wired the production CLI
    ///   dispatch (`db reset` replay path) to populate this field
    ///   from `crud_log_url` (env-var override or
    ///   derive-from-`database.url` fallback) via
    ///   [`super::resolve_audit_url`] + [`super::build_audit_pool`].
    ///   Tests that build `RunnerCtx` literals typically leave this
    ///   `None` — the dedicated audit-row coverage runs in
    ///   `tests/internal/sources/phase8_5_c2_118_*`.
    ///   **Why `deadpool_postgres::Pool` and not `DjogiPool`:** the
    ///   audit pool is not user-facing — adopters never see it, and
    ///   the runner constructs the audit-side `DjogiContext` itself
    ///   via `DjogiPool { inner: pool.clone() }` at the call site.
    ///   Holding the raw pool here keeps the dependency on
    ///   `DjogiPool`'s wider invariants (post-connect callbacks,
    ///   status reporting) out of `RunnerCtx`'s shape — those are
    ///   app-side concerns that do not apply to the audit DB.
    pub audit_pool: Option<deadpool_postgres::Pool>,
    /// Runner identity mode for this invocation. Controls whether
    /// and how node identity is bound on the pinned migration
    /// connection before non-Phase-0 run-id generation, user SQL,
    /// and ledger mutations.
    ///
    /// Library callers must pass an explicit value; the runtime
    /// library and pool constructors do not read `HEER_NODE_ID`.
    /// CLI callers resolve identity from `--node-id`, env
    /// `HEER_NODE_ID`, or `--single-node-dev` before constructing
    /// this context.
    ///
    /// **Legacy default.** When `None`, the runner treats this as
    /// `IdentityFree` for backward compatibility with existing test
    /// fixtures and library callers that have not yet migrated to
    /// explicit identity. New code should always pass a concrete
    /// value; the runner will validate identity-bearing operations
    /// refuse when no binding-capable mode is present.
    pub runner_identity: Option<RunnerIdentity>,
    /// Baseline for the apply-time drift pre-flight. Real CLI applies
    /// load this from the current on-disk snapshot; fake apply, reset
    /// replay, and baseline establishment deliberately disable it.
    pub drift_baseline: DriftBaseline,
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
    /// Number of metadata-only segments encountered. The runner records
    /// the count but does not execute filesystem moves; `compose` owns
    /// that path.
    pub metadata_segments: usize,
    /// Wall-clock elapsed time in milliseconds.
    pub execution_time_ms: i64,
}

#[expect(
    clippy::result_large_err,
    reason = "runner guard helpers return the public RunnerError enum unchanged"
)]
fn preflight_phase_zero_apply_artifact(
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
) -> Result<(), RunnerError> {
    if runner_ctx.version != super::bootstrap::PHASE_ZERO_VERSION {
        return Ok(());
    }

    let combined_up = plan
        .segments
        .iter()
        .flat_map(|seg| seg.statements.iter())
        .map(|stmt| stmt.up.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if let Err(refusal_reason) =
        super::phase_zero::require_identity_free_phase_zero_migration_artifact(
            combined_up.as_bytes(),
        )
    {
        return Err(RunnerError::StalePhaseZeroArtifact {
            version: runner_ctx.version.clone(),
            refusal_reason,
        });
    }
    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "runner guard helpers return the public RunnerError enum unchanged"
)]
fn verify_plan_checksum(plan: &MigrationPlan, runner_ctx: &RunnerCtx) -> Result<(), RunnerError> {
    let computed_up = compute_checksum_for_plan_up(plan);
    if let Err(e) =
        ledger::verify_checksum(&runner_ctx.version, &runner_ctx.checksum_up, &computed_up)
    {
        return Err(match e {
            VerifyError::Mismatch(m) => RunnerError::ChecksumMismatch(m),
            VerifyError::Format(f) => RunnerError::ChecksumFormat(f),
        });
    }
    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "runner guard helpers return the public RunnerError enum unchanged"
)]
fn validate_apply_identity(runner_ctx: &RunnerCtx) -> Result<(), RunnerError> {
    if runner_ctx.version == super::bootstrap::PHASE_ZERO_VERSION {
        return Ok(());
    }

    match runner_ctx.runner_identity {
        Some(identity) if identity.requires_binding() => Ok(()),
        _ => Err(RunnerError::MissingApplyIdentity {
            version: runner_ctx.version.clone(),
        }),
    }
}

#[expect(
    clippy::result_large_err,
    reason = "rollback guard helpers return the public RollbackError enum unchanged"
)]
fn validate_rollback_identity(runner_ctx: &RunnerCtx) -> Result<(), RollbackError> {
    if runner_ctx.version == super::bootstrap::PHASE_ZERO_VERSION {
        return Ok(());
    }

    match runner_ctx.runner_identity {
        Some(identity) if identity.requires_binding() => Ok(()),
        _ => Err(RollbackError::MissingRollbackIdentity {
            version: runner_ctx.version.clone(),
        }),
    }
}

// ── Public entry point ────────────────────────────────────────────────────

/// Apply a [`MigrationPlan`] against the runner's context.
/// **Witness-typed workspace lock.** The `_guard: &WorkspaceGuard`
/// parameter is a compile-time witness that the caller already holds
/// the workspace file lock — its mere presence at the type level
/// proves the lock is alive for the duration of the call. The runner
/// itself does not touch the guard; the parameter is named with a
/// leading underscore to signal "consumed at the type level only".
/// Misuse — calling `apply_plan` without first acquiring the lock
/// is a compile error rather than a silent race window. Tests that
/// only care about ledger semantics still go through the lock by
/// asking the [`super::guard::acquire`] helper for a per-test path.
/// **Three-database awareness.** The runner currently routes every
/// query through the supplied `&mut DjogiContext`'s pool. The
/// `RunnerCtx::bucket.database` field is hashed into the advisory
/// lock key and surfaced in operator-facing errors, but the actual
/// query routing follows the context the caller supplied. the
/// `DjogiContext` is single-pool today; when the three-database
/// `DjogiContext::pool_for(database)` API lands the runner will
/// pull the right pool from `runner_ctx.bucket.database` here. The
/// `BucketKey.database` channel exists today so the orchestrator
/// (`apply`) can construct one `DjogiContext` per database before
/// invoking the runner.
/// **Per-bucket advisory lock.** The runner DOES acquire and
/// release the per-bucket Postgres advisory lock around every
/// segment dispatch.
/// **Drift pre-flight.** Before inserting the pending ledger row or
/// running any migration SQL, apply verifies the live catalog against
/// the recorded snapshot baseline when the bucket already has applied
/// history.
/// **Snapshot persistence.** Writes the snapshot file ONLY after
/// the ledger row reaches `applied` and every segment succeeded.
/// Any failure leaves the snapshot at its prior value.
pub async fn apply_plan(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    _guard: &WorkspaceGuard,
) -> Result<RunReport, RunnerError> {
    preflight_phase_zero_apply_artifact(plan, runner_ctx)?;
    verify_plan_checksum(plan, runner_ctx)?;
    validate_apply_identity(runner_ctx)?;

    // GH #274 / #331 — pin one physical Postgres session for the
    // entire operation window so the advisory lock, DDL, ledger
    // writes, and lock release all run on the same backend.
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RunnerError::PinnedSessionCheckoutFailed { source: e })?;
    apply_plan_pinned(&mut pinned, plan, runner_ctx).await
}

/// Internal apply path that runs on an already-pinned context.
/// Both pool-backed (after checkout) and transaction-backed callers
/// route here. All queries in this function — bootstrap, advisory
/// lock, DDL, ledger writes, and unlock — run on the same physical
/// Postgres session.
async fn apply_plan_pinned(
    ctx: &mut PinnedCtx<'_>,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
) -> Result<RunReport, RunnerError> {
    // 1. Bootstrap the ledger table.
    ledger::bootstrap(ctx)
        .await
        .map_err(|e| RunnerError::LedgerBootstrapFailed { source: e })?;

    // 2. Acquire pg advisory lock for this bucket.
    let lock_key = advisory_lock_key(&plan.bucket);
    acquire_advisory_lock(ctx, &plan.bucket, lock_key).await?;

    // Whatever happens below, we must release the advisory lock.
    let result = apply_plan_inner(ctx, plan, runner_ctx).await;

    // 9. Always release advisory lock. Check the bool: false means the
    // lock was not held on this session (GH #274/#280).
    let released = release_advisory_lock(ctx, lock_key).await;

    let result = handle_release_result(result, released, &plan.bucket, lock_key);
    if result.is_ok() {
        ctx.mark_clean();
    }
    result
}

/// Core apply logic. Called from `apply_plan_pinned` after the advisory
/// lock is held. Returning early from any error path is fine; the outer
/// function's `release_advisory_lock` runs after.
async fn apply_plan_inner(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
) -> Result<RunReport, RunnerError> {
    let started = Instant::now();

    // PK-flip pre-flight: when the plan classifies as `PkTypeFlip`, run
    // the hazard checks (D060–D063) BEFORE any side effect. The
    // runner refuses on any hit with an actionable diagnostic; no
    // ledger row is inserted, no SQL runs.
    if matches!(
        plan.classification,
        super::diff::Classification::PkTypeFlip { .. }
    ) {
        pk_flip_preflight(ctx, runner_ctx, plan).await?;
    }

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

    // Out-of-order detection: walk the bucket's existing applied
    // ledger rows and surface any whose `version` is lexically greater
    // than ours — that indicates this version applies "before" a
    // previously-applied peer (the dev branch picked up a feature
    // branch's older migration after main shipped a newer one).
    // Lexical compare is correct because the version prefix is
    // `V<14 digits>` (timestamp-derived) so lexical order = chronological.
    // The detection runs BEFORE inserting the pending row so a
    // `Reject` policy leaves no database trace.
    let conflicting_peer = find_higher_applied_version(ctx, &plan.bucket, &runner_ctx.version)
        .await
        .map_err(|e| RunnerError::LedgerQueryFailed {
            query_label: "out_of_order_check",
            source: e,
        })?;
    let is_out_of_order = conflicting_peer.is_some();
    if is_out_of_order && !runner_ctx.out_of_order_policy.allows() {
        let (conflicting_version, conflicting_applied_at) =
            conflicting_peer.unwrap_or_else(|| (String::new(), None));
        return Err(RunnerError::OutOfOrderRejected {
            version: runner_ctx.version.clone(),
            conflicting_version,
            conflicting_applied_at,
        });
    }
    if is_out_of_order {
        let (conflicting_version, applied_at) = conflicting_peer
            .as_ref()
            .map(|(v, ts)| (v.as_str(), ts.as_deref()))
            .unwrap_or(("", None));
        tracing::warn!(
            bucket_database = %plan.bucket.database,
            bucket_app = %plan.bucket.app,
            version = %runner_ctx.version,
            conflicting_version,
            conflicting_applied_at = applied_at.unwrap_or("<unknown>"),
            policy = ?runner_ctx.out_of_order_policy,
            "out-of-order migration apply allowed by policy",
        );
    }
    match &runner_ctx.drift_baseline {
        DriftBaseline::Disabled => {}
        DriftBaseline::Missing | DriftBaseline::Corrupted(_) | DriftBaseline::Snapshot(_) => {
            let has_applied_history = bucket_has_applied_history(ctx, &plan.bucket)
                .await
                .map_err(|e| RunnerError::LedgerQueryFailed {
                    query_label: "drift_bucket_history",
                    source: e,
                })?;
            if has_applied_history {
                match &runner_ctx.drift_baseline {
                    DriftBaseline::Disabled => unreachable!("matched above"),
                    DriftBaseline::Missing => {
                        return Err(RunnerError::DriftBaselineMissing {
                            bucket: plan.bucket.clone(),
                        });
                    }
                    DriftBaseline::Corrupted(reason) => {
                        return Err(RunnerError::DriftBaselineCorrupted {
                            bucket: plan.bucket.clone(),
                            reason: reason.clone(),
                        });
                    }
                    DriftBaseline::Snapshot(snapshot) => {
                        let report = super::verify::verify_bucket(
                            ctx,
                            &plan.bucket,
                            snapshot,
                            &PolicyConfig::default(),
                            false,
                            false,
                        )
                        .await
                        .map_err(|source| {
                            RunnerError::DriftPreflightFailed {
                                source: Box::new(source),
                            }
                        })?;
                        if report.has_errors() {
                            return Err(RunnerError::DriftDetected {
                                bucket: plan.bucket.clone(),
                                report,
                            });
                        }
                    }
                }
            }
        }
    }
    // Compose a partial_apply_note when the policy is AllowExplicit so
    // the operator-supplied override reason lands on the row alongside
    // the out_of_order_flag. This is the audit-trail half of the
    // "tracing::warn! plus partial_apply_note" contract called out in
    // the out-of-order policy brief.
    let initial_note = compose_initial_note(
        is_out_of_order,
        runner_ctx.out_of_order_policy.override_reason(),
        conflicting_peer.as_ref(),
    );
    let durable_non_tx_note = initial_note.clone();

    // Expand `<EACH_LEAF_TABLE>` placeholders inside any
    // partitioned-flip segment before any runner-owned writes. This
    // produces the concrete SQL the runner will actually execute, so
    // every downstream preflight and step-count calculation works on
    // the real statement list.
    let (plan_owned, leaves_cache) =
        materialize_execution_plan(ctx, plan, PartitionExpansionMode::ApplyLenient).await?;
    let plan = &plan_owned;

    preflight_segment_sql_execution_compatibility(plan)?;

    // Determine execution_mode + total_steps from the concrete plan
    // shape. `total_steps` counts non-transactional steps (each of
    // which is its own resumable unit). Transactional segments
    // collapse to a single atomic step from the resumability
    // perspective.
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

    // 5. Gate: non-Phase-0 apply requires a binding-capable runner
    // identity before `generate_run_id()` (which invokes HeeRanjID-
    // backed SQL). Mirrors the rollback inverse refusal at
    // [`rollback_handle_lock`]. Phase 0 remains exempt.
    if runner_ctx.version != super::bootstrap::PHASE_ZERO_VERSION {
        let identity = match runner_ctx.runner_identity {
            Some(id) => id,
            None => {
                return Err(RunnerError::MissingApplyIdentity {
                    version: runner_ctx.version.clone(),
                });
            }
        };
        if !identity.requires_binding() {
            return Err(RunnerError::MissingApplyIdentity {
                version: runner_ctx.version.clone(),
            });
        }

        // Bind the selected node identity on the pinned connection.
        bind_runner_node_identity(
            ctx,
            identity.node_id().expect(
                "INVARIANT: node_id is Some when requires_binding is true; \
                 IdentityFree ruled out by refusal gate above",
            ),
        )
        .await?;
    }

    // 5b. Insert pending ledger row. Generate a fresh run_id.
    let run_id = generate_run_id(ctx, &runner_ctx.version).await?;
    let ledger_row = LedgerRow {
        version: runner_ctx.version.clone(),
        description: runner_ctx.description.clone(),
        checksum_up: runner_ctx.checksum_up.clone(),
        checksum_down: runner_ctx.checksum_down.clone(),
        execution_mode,
        status: LedgerStatus::Pending,
        execution_time_ms: 0,
        out_of_order_flag: is_out_of_order,
        applied_steps_count: 0,
        total_steps: if has_non_tx {
            Some(total_non_tx_steps)
        } else {
            None
        },
        partial_apply_note: initial_note,
        run_id,
        snapshot_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        app_label: plan.bucket.app.clone(),
        leaf_identity: serialize_leaf_identity(&leaves_cache),
    };
    let ledger_id = match ledger::insert_pending(ctx, &ledger_row).await {
        Ok(id) => id,
        Err(e) => {
            if is_unique_violation(&e) {
                return Err(classify_duplicate_version_collision(
                    ctx,
                    &runner_ctx.version,
                    &runner_ctx.bucket.app,
                )
                .await);
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
                    run_transactional_segment(ctx, segment, seg_idx, runner_ctx, &add_table_set)
                        .await
                {
                    // The relpages probe runs BEFORE BEGIN, so
                    // a probe failure must NOT be reported as a
                    // transactional-segment failure (the tx has not
                    // even opened yet). Distinguish probe-side
                    // failures from in-tx statement failures so the
                    // operator-facing note matches the actual phase
                    // that failed.
                    let note = note_for_failed_transactional_segment(seg_idx, &e);
                    let _ = ledger::mark_failed(ctx, ledger_id, &note).await;
                    return Err(e);
                }
                transactional_segments += 1;
            }
            SegmentKind::NonTransactional => {
                match run_non_transactional_segment(
                    ctx,
                    segment,
                    NonTransactionalSegmentRun {
                        segment_index: seg_idx,
                        version: &runner_ctx.version,
                        ledger_id,
                        prior_steps_completed: applied_non_tx_steps,
                        total_non_tx_steps,
                        stable_note: durable_non_tx_note.as_deref(),
                        runner_ctx,
                    },
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
                // The runner records the segment count but does not execute
                // filesystem moves — that is `apply`'s
                // orchestrator. The presence of metadata-only
                // segments is captured in the ledger via the count
                // tracked here and returned in the RunReport.
                metadata_segments += 1;
            }
        }
    }

    if runner_ctx.version == super::bootstrap::PHASE_ZERO_VERSION
        && runner_ctx
            .runner_identity
            .is_some_and(RunnerIdentity::provisions_phase_zero_single_node_dev)
    {
        let node_id = super::bootstrap::DEFAULT_NODE_ID;
        if let Err(e) = provision_phase_zero_single_node_dev(ctx, node_id).await {
            let note = format!("single-node-dev provisioning failed before mark_applied: {e}");
            let _ = ledger::mark_failed(ctx, ledger_id, &note).await;
            return Err(e);
        }
    }

    // 7. Mark applied + persist snapshot. Phase 0 SingleNodeDev
    // provisioning has already succeeded if it was required. The order is:
    // a. Write the snapshot file.
    // b. Mark the ledger row applied.
    // If (a) fails, the ledger stays `pending` and the operator can
    // inspect the partial state. If (b) fails after (a) succeeded,
    // the snapshot is on disk but the ledger says pending — also a
    // recoverable state because the runner's idempotency check
    // `apply` sees the snapshot match the descriptor and treats it as
    // already-applied.
    // However: per the v3 plan's hard invariant ("snapshot moves
    // forward AFTER ledger reaches applied"), we flip the order:
    // a. Mark the ledger row applied.
    // b. Write the snapshot file.
    // If (b) fails, the operator runs `compose` to regenerate the
    // snapshot from the descriptor inventory.
    let elapsed_ms: i64 = elapsed_ms(started);

    // When the row was flagged out-of-order, preserve the
    // partial_apply_note so the historical conflict + override reason
    // stay visible alongside `applied` status. The default
    // `mark_applied` clears the note (its prior purpose was to
    // describe a partial-apply state that resolves on success).
    if is_out_of_order {
        ledger::mark_applied_keep_note(ctx, ledger_id, elapsed_ms, applied_non_tx_steps)
            .await
            .map_err(|e| RunnerError::LedgerWriteFailed {
                version: runner_ctx.version.clone(),
                source: e,
            })?;
    } else {
        ledger::mark_applied(ctx, ledger_id, elapsed_ms, applied_non_tx_steps)
            .await
            .map_err(|e| RunnerError::LedgerWriteFailed {
                version: runner_ctx.version.clone(),
                source: e,
            })?;
    }

    if let (Some(snapshot), Some(path)) = (&runner_ctx.snapshot, &runner_ctx.snapshot_path) {
        save_snapshot(snapshot, path).map_err(|e| RunnerError::SnapshotPersistFailed {
            path: path.clone(),
            source: e,
        })?;
    }

    // #118 — DDL audit. Best-effort: if any
    // audit-side step fails we log via `tracing::warn!` and SKIP. The
    // app DB DDL has already succeeded; an audit-DB outage MUST NOT
    // roll back work that already committed. See
    // `record_ddl_audit_for_plan` for the full failure-mode
    // rationale.
    // **Snapshot decoupling (issue #118).** The audit-write loop runs
    // whenever `audit_pool.is_some()`, regardless of whether a
    // snapshot was persisted on this apply. The snapshot signature
    // becomes part of the audit row only when a snapshot was just
    // written; the `db reset` replay path deliberately passes
    // `snapshot: None` (the on-disk snapshot is unchanged across the
    // drop / recreate / replay cycle) and would otherwise have its
    // audit row suppressed by an unrelated pre-condition. The audit
    // row's primary purpose — recording that a migration's DDL ran
    // is independent of whether new schema bytes hit disk.
    record_ddl_audit_for_plan(plan, runner_ctx, runner_ctx.snapshot.as_ref()).await;

    Ok(RunReport {
        ledger_id,
        run_id,
        transactional_segments,
        non_transactional_segments,
        metadata_segments,
        execution_time_ms: elapsed_ms,
    })
}

/// Write one `djogi_ddl_audit` row per executed (non-metadata-only)
/// segment in the plan. Snapshot decoupling issue #118.
/// # Why this lives on the success-only path
/// The caller invokes this AFTER:
/// 1. Every segment has committed (transactional or non-transactional).
/// 2. `mark_applied` flipped the ledger row to `applied`.
/// 3. `save_snapshot` persisted the new schema-of-record to disk
///    (when a snapshot was supplied by the caller; the `db reset`
///    replay path deliberately does not).
///    Calling it earlier would risk an audit row whose
///    `snapshot_signature_hex` does not correspond to any persisted
///    snapshot — the signature would be of an in-memory `AppliedSchema`
///    that never reached disk. Per v3 plan §453 the audit row's purpose
///    is to ground the migration trail to the schema-of-record file
///    `djogi verify` inspects.
/// # Snapshot is optional (#118)
/// The `snapshot` parameter is `Option<&AppliedSchema>`. The audit
/// row's primary purpose — recording that a migration's DDL ran — is
/// independent of whether new schema bytes hit disk on this apply:
/// - **`Some(snapshot)`** — the runner just persisted these bytes to
///   `snapshot_path`. The audit row's `snapshot_signature_hex` is
///   the HMAC over those bytes (or the no-op zero hex when the
///   signing key is unset).
/// - **`None`** — no snapshot was persisted (e.g. `db reset` replay,
///   which re-runs the existing migrations against a fresh DB
///   without producing new schema bytes). The audit row carries
///   `NULL` in the signature column. NULL distinguishes "no snapshot
///   was written this apply" from "snapshot written, signed under
///   the no-op key" (the latter produces 64 zero hex chars).
///   Pre-issue-#118 this function took `&AppliedSchema` and the call
///   site gated audit writes on snapshot presence — which meant `db
/// reset` (the only production constructor of `RunnerCtx`) could
///   never write audit rows.
/// # Three-database awareness
/// The audit DB is operationally separate from the app DB
/// (`crud_log_url` vs. `url`). We construct a fresh
/// [`DjogiContext`] from `runner_ctx.audit_pool` — NEVER from the
/// app-side context — so a query routed here cannot accidentally
/// run against the app DB. See CLAUDE.md "Three-Database
/// Architecture".
/// # Failure-mode rationale
/// Three steps can fail: the audit-DDL bootstrap, individual
/// `INSERT` calls, and the implicit pool checkout each `DjogiContext`
/// helper performs. ALL THREE log via `tracing::warn!` and SKIP
/// none propagate. Reasons:
/// - The app DB DDL has already committed.
/// - The on-disk snapshot has already been persisted (when supplied).
/// - The ledger row has already reached `applied`.
///   Rolling any of those back because the audit DB is unreachable
///   would be a worse outcome than a missing audit row. Operators
///   rebuilding the audit trail can replay from the ledger + snapshot;
///   they cannot recover from a runner that refused to record an
///   otherwise-clean migration apply because a sibling DB happened to
///   be down.
/// # Per-segment rows
/// One row per executed segment so the audit trail captures the same
/// granularity the runner reports (the `Transactional /
/// NonTransactional` split). MetadataOnly segments produce no SQL so
/// they are skipped — there is no `ddl_sql` to record. The same
/// `snapshot_signature_hex` (or `NULL` when no snapshot was supplied)
/// lands on every segment row from a single apply: rows from one
/// apply share the post-apply schema-of-record, and the audit reader
/// reconstructs the per-segment timeline from `applied_at` ordering
/// on the ledger side.
/// # Signing key
/// `DJOGI_SNAPSHOT_SIGNING_KEY` unset OR malformed → silently
/// degrades to the no-op key `[0u8; 32]`. The runner intentionally
/// does NOT log on malformed input; the CLI entry point that sets
/// up signing owns the operator-facing surface for key errors
/// (`djogi verify` surfaces them as `VerifyError::KeyDecode`).
/// Signing under the no-op key produces `[0u8; 32]` → 64 zero hex
/// chars → a non-NULL string in the audit column. `NULL` is reserved
/// for code paths where no snapshot was supplied at all (today:
/// `db reset` replay).
fn audit_signing_key_from_loaded(
    loaded: Result<Option<[u8; 32]>, crate::snapshot::sign::SnapshotKeyError>,
) -> [u8; 32] {
    loaded.ok().flatten().unwrap_or([0u8; 32])
}

fn audit_signature_hex_for_snapshot(
    snapshot: &super::schema::AppliedSchema,
    key: [u8; 32],
) -> Result<String, SnapshotError> {
    let snapshot_bytes = super::snapshot::serialize_snapshot(snapshot)?;
    let sig = crate::snapshot::sign::sign_snapshot(&snapshot_bytes, &key);
    Ok(super::audit::signature_to_hex(&sig))
}

async fn record_ddl_audit_for_plan(
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    snapshot: Option<&super::schema::AppliedSchema>,
) {
    let Some(audit_pool) = runner_ctx.audit_pool.as_ref() else {
        // Audit pool not configured — this is the supported "no
        // audit DB" deployment shape. Silent skip is correct.
        return;
    };

    // Resolve the signing key before rendering the signature. Per the
    // no-op-key sentinel contract in `snapshot::sign`, an unset env var
    // (`Ok(None)`) and malformed value (`Err`) both collapse here to the
    // no-op key. The CLI entry point that sets the env var owns the
    // operator-facing surface for malformed keys (`djogi verify` surfaces
    // them as `VerifyError::KeyDecode`); the runner is the audit-side
    // consumer, not the configuration owner.
    // **Snapshot decoupling (#118).** Audit rows now
    // fire whenever `audit_pool.is_some()` regardless of whether a
    // snapshot was persisted on this apply (the runner's `db reset`
    // replay path deliberately passes `snapshot: None`). When no
    // snapshot is supplied the audit row's `snapshot_signature_hex`
    // column is `NULL` — distinguishing "no snapshot was written" from
    // "snapshot written, signed under the no-op key" (the latter
    // produces 64 zero hex chars, the former is `NULL`). This matches
    // the `record_ddl` parameter contract: `snapshot_sig_hex:
    // Option<&str>` was always optional; pre-issue-#118 the call site
    // happened to always pass `Some`, but the column is nullable by
    // design.
    let key = audit_signing_key_from_loaded(crate::snapshot::sign::load_signing_key_from_env());
    let sig_hex_opt: Option<String> = match snapshot {
        Some(s) => match audit_signature_hex_for_snapshot(s, key) {
            Ok(sig_hex) => Some(sig_hex),
            Err(e) => {
                tracing::warn!(
                    target: "djogi::migrate::audit",
                    error = ?e,
                    "snapshot re-serialisation for audit signature failed; \
                     proceeding with NULL signature so the DDL audit row still records the apply",
                );
                None
            }
        },
        None => None,
    };

    // Construct an audit-side DjogiContext. `DjogiPool` wraps the
    // raw deadpool pool; `inner` is `pub(crate)`, so this lives in
    // the same crate as `pg::pool`. We clone the pool handle (an
    // `Arc` bump under the hood — zero cost) so the runner does not
    // disturb the caller's ownership of `runner_ctx.audit_pool`.
    let audit_djogi_pool = crate::pg::pool::DjogiPool {
        inner: audit_pool.clone(),
        // Audit pools don't surface a URL — internal substrate only,
        // never reaches the NOTIFY subscriber path. See `DjogiPool::url`
        // doc for the contract.
        url: None,
        // Fresh per-process id every time the runner constructs an
        // audit-side `DjogiPool`, so this transient handle never
        // collides with any other pool's NOTIFY-registry slot.
        pool_id: crate::pg::pool::next_pool_id(),
    };
    let mut audit_ctx = DjogiContext::from_pool(audit_djogi_pool);

    // Bootstrap the audit table — `CREATE TABLE IF NOT EXISTS` is
    // idempotent so calling on every apply is cheap. Doing it here
    // (rather than once at process start) keeps the runner
    // self-contained: no separate "init audit DB" CLI step is
    // required.
    if let Err(e) = super::audit::bootstrap_ddl_audit(&mut audit_ctx).await {
        tracing::warn!(
            target: "djogi::migrate::audit",
            bucket_database = %plan.bucket.database,
            bucket_app = %plan.bucket.app,
            error = ?e,
            "djogi_ddl_audit bootstrap failed; skipping audit rows for this apply",
        );
        return;
    }

    // One row per executed segment. MetadataOnly segments carry no
    // SQL — the apply path skips them and the audit trail follows.
    for (seg_idx, segment) in plan.segments.iter().enumerate() {
        if segment.kind == SegmentKind::MetadataOnly {
            continue;
        }
        // Concatenate the `up` SQL for this segment in execution
        // order, separated by `;\n`. Storing the concatenated text
        // (rather than one row per statement) matches the segment
        // granularity the runner already reports and keeps the
        // audit table at one row per atomic-commit unit.
        let ddl_sql: String = segment
            .statements
            .iter()
            .map(|s| s.up.as_str())
            .collect::<Vec<_>>()
            .join(";\n");
        // Routes through the public re-export `record_ddl_audit`
        // (the in-module fn is `audit::record_ddl`; the re-export
        // adds the `_audit` suffix to disambiguate from sibling
        // ledger / seed CRUD helpers — see the naming-disambiguation note on
        // naming drift). Calling the re-export keeps the runner
        // aligned with the public-surface name even though the
        // private path would also resolve.
        if let Err(e) = super::record_ddl_audit(
            &mut audit_ctx,
            &plan.bucket.database,
            &plan.bucket.app,
            &ddl_sql,
            sig_hex_opt.as_deref(),
        )
        .await
        {
            tracing::warn!(
                target: "djogi::migrate::audit",
                bucket_database = %plan.bucket.database,
                bucket_app = %plan.bucket.app,
                segment_index = seg_idx,
                error = ?e,
                "djogi_ddl_audit insert failed; continuing with remaining segments",
            );
            // Continue — best-effort. A transient error on one
            // segment row should not suppress the next.
        }
    }
}

// ── Rollback / fake-apply / baseline ─────────────────

/// Operator-supplied policy for a rollback whose `down` SQL is
/// flagged lossy by the SQL emitter (e.g. `DropColumn`, `DropTable`,
/// `DropEnum`, `DropIndex`).
/// Lossy rollback means the down SQL cannot reconstruct the original
/// row data — for `DropColumn` the column is gone, for `DropTable`
/// the rows are gone, for `DropEnum` the type's existence is gone.
/// Rollback refuses to run a lossy operation by default; the operator
/// must explicitly opt in via `LossyRollbackPolicy::Allow { reason }`.
/// The reason is recorded verbatim in the ledger row's
/// `partial_apply_note` so the audit trail captures *why* the loss
/// was acceptable.
#[derive(Debug, Clone)]
pub enum LossyRollbackPolicy {
    /// Refuse to run any lossy down statement. The default and the
    /// safe choice — verifying the rollback is a non-event before
    /// proceeding.
    Refuse,
    /// Run lossy down statements anyway. The `reason` field is
    /// preserved into the ledger's `partial_apply_note` for audit.
    Allow {
        /// Operator-supplied rationale; non-empty by convention. The
        /// rollback path does not enforce non-emptiness so dev
        /// iterations can pass `String::new()`, but production
        /// callers should always set a real string.
        reason: String,
    },
}

/// SQL file side involved in rollback checksum parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackChecksumSide {
    Up,
    Down,
}

impl RollbackChecksumSide {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// Errors specific to [`rollback_plan`]. Distinct from [`RunnerError`]
/// because rollback shares many shapes with apply but adds the
/// lossy-policy refusal path.
#[derive(Debug)]
pub enum RollbackError {
    /// Workspace lock / Postgres error from the apply substrate.
    /// Wraps [`RunnerError`] so the caller can match the specific
    /// underlying failure.
    Runner {
        source: RunnerError,
        /// `true` when the rollback may already have mutated the
        /// live database before this runner-level error surfaced.
        live_db_committed: bool,
    },
    /// At least one operation's `down` SQL is flagged lossy and the
    /// operator did not opt in. The rollback was rejected before any
    /// SQL ran.
    LossyRollbackRefused {
        /// Operation labels carrying a lossy marker.
        offending_labels: Vec<String>,
        /// Per-label loss kind so the operator-facing message names
        /// the categories.
        kinds: Vec<LossyRollbackKind>,
    },
    /// The version is not present in the ledger or already in a
    /// status that admits no rollback (`pending`, `failed`,
    /// `rolled_back`, `baseline`).
    VersionNotRollbackable {
        version: String,
        current_status: LedgerStatus,
    },
    /// The version was not found in the ledger at all.
    VersionNotFound { version: String },
    /// The plan's `bucket.app` does not match the ledger row's
    /// `app_label`. The advisory lock is acquired on `plan.bucket`
    /// before the row is loaded; if they differ the runner would
    /// mutate the row while holding a lock for the wrong logical
    /// bucket. Rollback refuses and releases the lock on the same
    /// pinned session before returning (GH #274 hardening).
    BucketAppMismatch {
        /// The migration version whose row was loaded.
        version: String,
        /// The `app_label` stored in the ledger row.
        row_app_label: String,
        /// The `bucket.app` from the caller-supplied plan.
        supplied_app: String,
    },
    /// A `down` statement raised a Postgres error mid-rollback.
    DownStatementFailed {
        segment_index: usize,
        statement_label: String,
        /// `true` when the failed statement may already have
        /// committed or left residue in the live database.
        live_db_committed: bool,
        source: DjogiError,
    },
    /// The applied ledger row's checksums do not match the current
    /// file-derived rollback context. Refused before any down SQL
    /// runs; operators should restore the exact migration files or
    /// explicitly repair checksum drift before rollback.
    ChecksumDrift {
        version: String,
        side: RollbackChecksumSide,
        ledger: Option<String>,
        on_disk: Option<String>,
    },
    /// The runner was asked to revert the snapshot to a prior version
    /// but no `prior_snapshot` was supplied.
    PriorSnapshotMissing,
    /// **#356** — The freshly materialized leaf identity for this migration
    /// does not match the value stored in the ledger. The partition topology
    /// has changed since the original apply, so rolling back against different
    /// leaves would be unsafe.
    LeafIdentityMismatch {
        /// Migration version being rolled back.
        version: String,
        /// Leaf identity stored in the ledger row from the original apply.
        stored_leaf_identity: String,
        /// Leaf identity recomputed from the current live partition topology.
        current_leaf_identity: String,
    },
    /// I/O failure persisting the prior snapshot back to disk.
    SnapshotPersistFailed {
        path: PathBuf,
        source: SnapshotError,
    },

    /// **#386** — Phase 0 rollback down payload was not classified as
    /// identity-free replay-current before any down SQL execution. The
    /// runner refuses `seed-capable-runtime-only`,
    /// `seed-dml-not-runtime-current`, `generated-stale`, `ambiguous`,
    /// `incomplete`, or `missing` down payloads before touching the
    /// ledger, snapshot, or database.
    StalePhaseZeroDown {
        /// Migration version being rolled back.
        version: String,
        /// Classification refusal reason from the Phase 0 classifier.
        refusal_reason: &'static str,
    },

    /// **#386** — Non-Phase-0 rollback reached SQL execution without
    /// a binding-capable runner identity. `IdentityFree` and missing
    /// identity (`None`) are refused for non-Phase-0 rollback because
    /// down SQL executes user migration statements and generates run
    /// IDs via HeeRanjID-backed functions.
    MissingRollbackIdentity {
        /// Migration version being rolled back.
        version: String,
    },
}

impl RollbackError {
    fn runner(source: RunnerError) -> Self {
        Self::Runner {
            source,
            live_db_committed: false,
        }
    }

    fn runner_committed(source: RunnerError) -> Self {
        Self::Runner {
            source,
            live_db_committed: true,
        }
    }

    /// Return `true` when this rollback error occurred after the
    /// live database may already have been mutated.
    ///
    /// A successful rollback result means the live DB is committed by
    /// definition. This method exists for `Err` paths, where callers
    /// need to decide whether automatic retry is safe.
    #[must_use]
    pub fn live_db_committed(&self) -> bool {
        match self {
            Self::Runner {
                live_db_committed, ..
            }
            | Self::DownStatementFailed {
                live_db_committed, ..
            } => *live_db_committed,
            Self::SnapshotPersistFailed { .. } => true,
            Self::LossyRollbackRefused { .. }
            | Self::VersionNotRollbackable { .. }
            | Self::VersionNotFound { .. }
            | Self::BucketAppMismatch { .. }
            | Self::ChecksumDrift { .. }
            | Self::PriorSnapshotMissing
            | Self::LeafIdentityMismatch { .. }
            | Self::StalePhaseZeroDown { .. }
            | Self::MissingRollbackIdentity { .. } => false,
        }
    }
}

fn render_rollback_checksum_value(value: Option<&str>) -> &str {
    value.unwrap_or("<none>")
}

impl std::fmt::Display for RollbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RollbackError::Runner { source, .. } => {
                write!(f, "rollback failed at runner level: {source}")
            }
            RollbackError::LossyRollbackRefused {
                offending_labels, ..
            } => write!(
                f,
                "rollback refused: {n} operation(s) carry a lossy down side; \
                 supply LossyRollbackPolicy::Allow {{ reason }} to proceed: {labels:?}",
                n = offending_labels.len(),
                labels = offending_labels,
            ),
            RollbackError::VersionNotRollbackable {
                version,
                current_status,
            } => write!(
                f,
                "version `{version}` is not in a rollbackable status (current: {current})",
                current = current_status.as_db_str(),
            ),
            RollbackError::VersionNotFound { version } => {
                write!(f, "version `{version}` is not present in the ledger")
            }
            RollbackError::BucketAppMismatch {
                version,
                row_app_label,
                supplied_app,
            } => write!(
                f,
                "rollback rejected: version `{version}` belongs to app \
                 `{row_app_label}` but the supplied plan has bucket app \
                 `{supplied_app}`; the advisory lock would be held for \
                 the wrong logical bucket",
            ),
            RollbackError::DownStatementFailed {
                segment_index,
                statement_label,
                source,
                ..
            } => write!(
                f,
                "rollback `down` segment {segment_index} `{statement_label}` failed: {source}",
            ),
            RollbackError::ChecksumDrift {
                version,
                side,
                ledger,
                on_disk,
            } => {
                write!(
                    f,
                    "rollback refused: {side} checksum drift for `{version}` \
                     (ledger={} on_disk={}); restore the exact committed migration \
                     files or run `djogi migrations repair checksum-drift` before rollback",
                    render_rollback_checksum_value(ledger.as_deref()),
                    render_rollback_checksum_value(on_disk.as_deref()),
                    side = side.as_str(),
                )
            }
            RollbackError::PriorSnapshotMissing => f.write_str(
                "rollback requires a prior_snapshot to revert to but the caller passed None",
            ),
            RollbackError::SnapshotPersistFailed { path, source } => {
                write!(
                    f,
                    "rollback snapshot persist at {} failed: {source}",
                    path.display()
                )
            }
            RollbackError::LeafIdentityMismatch {
                version,
                stored_leaf_identity: _,
                current_leaf_identity: _,
            } => write!(
                f,
                "[D624] rollback refused: partition leaf identity mismatch for `{version}`",
            ),
            RollbackError::StalePhaseZeroDown {
                version,
                refusal_reason,
            } => write!(
                f,
                "rollback refused: Phase 0 down SQL for `{version}` is {refusal_reason}; \
                 refusing before execution to prevent stale artifact mutation",
            ),
            RollbackError::MissingRollbackIdentity { version } => write!(
                f,
                "rollback refused: non-Phase-0 rollback of `{version}` requires a \
                 binding-capable runner identity (Selected or SingleNodeDev); \
                 IdentityFree and missing identity are not allowed for down SQL execution",
            ),
        }
    }
}

impl std::error::Error for RollbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RollbackError::Runner { source, .. } => Some(source),
            RollbackError::DownStatementFailed { source, .. } => Some(source),
            RollbackError::SnapshotPersistFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Roll back a previously-applied migration by running its emitted
/// `down` SQL in reverse order.
/// **Witness-typed workspace lock.** Like [`apply_plan`], rollback
/// requires a `&WorkspaceGuard` so the file lock is held for the
/// entire operation.
/// **Order of execution.** Apply runs transactional segments first
/// then non-transactional. Rollback inverts that:
/// 1. Non-transactional segments run first, in reverse statement
///    order, autocommitting each step. (Apply's order is
///    NonTx-segment-after-Tx-segment by segment-list position; the
///    rollback walks segments in *reverse list position* and reverses
///    each segment's statements.)
/// 2. Transactional segments run last, all wrapped in one Postgres
///    transaction so a partial-rollback failure rolls back cleanly.
///    **Lossy down handling.** Pre-walks the plan and collects every
///    operation whose `lossy.is_some()`. With
///    [`LossyRollbackPolicy::Refuse`] (the default), surfaces the list
///    as [`RollbackError::LossyRollbackRefused`] before any SQL runs.
///    With [`LossyRollbackPolicy::Allow { reason }`], the rollback
///    proceeds and the reason is preserved in the ledger row's
///    `partial_apply_note`.
///    **Snapshot semantics.** Rollback does NOT re-derive the prior
///    snapshot from the down SQL — that requires a full delta-replay
///    engine which is outside the rollback scope. The caller (typically
///    `apply` orchestrator with a snapshot history) supplies the prior
///    snapshot explicitly via `prior_snapshot` and `prior_snapshot_path`.
///    Pass `None` to skip the snapshot revert (tests, when the snapshot
///    is not under test).
///    **Ledger update.** On success, the ledger row's status flips to
///    [`LedgerStatus::RolledBack`], `applied_steps_count` resets to 0,
///    and `partial_apply_note` is filled with a record of the rollback
///    (timestamp + lossy reason if any).
///
/// **Recovering from a failed rollback.** An `Ok(`[`RollbackReport`]`)`
/// means every segment committed and any caller-supplied snapshot was
/// persisted — no recovery is needed. On the `Err` arm, call
/// [`RollbackError::live_db_committed`] before retrying: when it returns
/// `true` the live database was already mutated (for example, a
/// non-transactional segment auto-committed before a later transactional
/// segment failed), so the caller must rebuild any derived state — the
/// schema snapshot in particular — to match the partially-applied reality
/// before issuing another `rollback_plan`. Retrying blindly after a
/// post-commit failure will surface
/// [`RollbackError::VersionNotRollbackable`], because the ledger row has
/// already advanced past a rollbackable status.
pub async fn rollback_plan(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    _guard: &WorkspaceGuard,
    lossy_policy: LossyRollbackPolicy,
    prior_snapshot: Option<&super::schema::AppliedSchema>,
) -> Result<RollbackReport, RollbackError> {
    // Hoist the PriorSnapshotMissing check to the very top
    // before any ledger bootstrap, before any DDL, before any ledger
    // mutation.
    if prior_snapshot.is_none() && runner_ctx.snapshot_path.is_some() {
        return Err(RollbackError::PriorSnapshotMissing);
    }
    validate_rollback_identity(runner_ctx)?;

    // GH #274 / #331 — pin one physical Postgres session for the
    // entire rollback window (same contract as apply_plan).
    let mut pinned = ctx.pin_for_migration().await.map_err(|e| {
        RollbackError::runner(RunnerError::PinnedSessionCheckoutFailed { source: e })
    })?;
    rollback_plan_pinned(&mut pinned, plan, runner_ctx, lossy_policy, prior_snapshot).await
}

/// Determine whether lossy rollback is permitted for the given plan.
/// Scans all statements in the plan for lossy markers and enforces
/// the policy. Returns the operator-supplied reason string (if any)
/// when lossy operations are present but allowed, or None if no
/// lossy operations exist. Refuses with [`RollbackError`] when
/// lossy operations are present and the policy is `Refuse`.
/// This operates against the materialized replay plan (partition-expanded),
/// not the original unexpanded plan, so per-leaf lossy markers are properly
/// detected (GH #317).
// RollbackError is a large enum by design; this sync helper returns it by
// value and needs the suppression. Same rationale as handle_release_result.
#[allow(clippy::result_large_err)]
fn rollback_lossy_allow_reason(
    plan: &MigrationPlan,
    lossy_policy: &LossyRollbackPolicy,
) -> Result<Option<String>, RollbackError> {
    let lossy_ops: Vec<(String, LossyRollbackKind)> = plan
        .segments
        .iter()
        .flat_map(|s| s.statements.iter())
        .filter_map(|stmt| stmt.lossy.as_ref().map(|w| (stmt.label.clone(), w.kind)))
        .collect();

    match (lossy_policy, lossy_ops.is_empty()) {
        (LossyRollbackPolicy::Refuse, true) => Ok(None),
        (LossyRollbackPolicy::Refuse, false) => Err(RollbackError::LossyRollbackRefused {
            offending_labels: lossy_ops.iter().map(|(label, _)| label.clone()).collect(),
            kinds: lossy_ops.iter().map(|(_, kind)| *kind).collect(),
        }),
        (LossyRollbackPolicy::Allow { reason }, _) => Ok(Some(reason.clone())),
    }
}

#[expect(
    clippy::result_large_err,
    reason = "rollback guard helpers return the public RollbackError enum unchanged"
)]
fn preflight_phase_zero_down_payload(
    replay_plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
) -> Result<(), RollbackError> {
    if runner_ctx.version != super::bootstrap::PHASE_ZERO_VERSION {
        return Ok(());
    }

    let down_sqls = replay_plan
        .segments
        .iter()
        .flat_map(|seg| seg.statements.iter())
        .map(|stmt| stmt.down.as_str());

    if let Err(refusal_reason) =
        super::phase_zero::require_identity_free_phase_zero_down_payload(down_sqls)
    {
        return Err(RollbackError::StalePhaseZeroDown {
            version: runner_ctx.version.clone(),
            refusal_reason,
        });
    }
    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "rollback guard helpers return the public RollbackError enum unchanged"
)]
fn validate_rollback_checksum_parity(
    row: &LedgerRow,
    runner_ctx: &RunnerCtx,
) -> Result<(), RollbackError> {
    if row.checksum_up != runner_ctx.checksum_up {
        return Err(RollbackError::ChecksumDrift {
            version: runner_ctx.version.clone(),
            side: RollbackChecksumSide::Up,
            ledger: Some(row.checksum_up.clone()),
            on_disk: Some(runner_ctx.checksum_up.clone()),
        });
    }
    if row.checksum_down != runner_ctx.checksum_down {
        return Err(RollbackError::ChecksumDrift {
            version: runner_ctx.version.clone(),
            side: RollbackChecksumSide::Down,
            ledger: row.checksum_down.clone(),
            on_disk: runner_ctx.checksum_down.clone(),
        });
    }
    Ok(())
}

/// Internal rollback path that runs on an already-pinned context.
/// Materializes the strict replay plan inside the advisory lock before
/// scanning lossy markers or executing down SQL. Partition-expanded
/// statements are rematerialized from the same execution stream used
/// by apply; rollback replays reverse down SQL against the expanded
/// stream, ensuring per-leaf lossy markers are properly detected
/// (GH #317).
async fn rollback_plan_pinned(
    ctx: &mut PinnedCtx<'_>,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    lossy_policy: LossyRollbackPolicy,
    prior_snapshot: Option<&super::schema::AppliedSchema>,
) -> Result<RollbackReport, RollbackError> {
    // 1. Bootstrap the ledger so the SELECT below cannot fail with
    // relation-not-found.
    ledger::bootstrap(ctx)
        .await
        .map_err(|e| RollbackError::runner(RunnerError::LedgerBootstrapFailed { source: e }))?;

    // 2. Acquire the per-bucket advisory lock BEFORE loading the ledger
    // row. Moving the row read inside the lock eliminates the TOCTOU
    // window between the status check and the down-SQL mutations
    // (GH #274).
    let lock_key = advisory_lock_key(&plan.bucket);
    acquire_advisory_lock(ctx, &plan.bucket, lock_key)
        .await
        .map_err(RollbackError::runner)?;

    let result = rollback_handle_lock(ctx, plan, runner_ctx, lossy_policy, prior_snapshot).await;

    let released = release_advisory_lock(ctx, lock_key).await;

    // Mirror handle_release_result for the RollbackError type.
    match (result, released) {
        (Ok(r), true) => {
            ctx.mark_clean();
            Ok(r)
        }
        (Ok(_), false) => Err(RollbackError::runner_committed(
            RunnerError::AdvisoryUnlockReturnedFalse {
                key: lock_key,
                bucket: plan.bucket.clone(),
            },
        )),
        (Err(e), _) => Err(e),
    }
}

async fn rollback_handle_lock(
    ctx: &mut PinnedCtx<'_>,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    lossy_policy: LossyRollbackPolicy,
    prior_snapshot: Option<&super::schema::AppliedSchema>,
) -> Result<RollbackReport, RollbackError> {
    // 3. Confirm the row exists and is in a rollbackable status — inside
    // the lock so the status read is atomic with the subsequent write.
    let row = load_ledger_row_for_version(ctx, &runner_ctx.version, &runner_ctx.bucket.app)
        .await
        .map_err(|e| {
            RollbackError::runner(RunnerError::LedgerQueryFailed {
                query_label: "load_row_for_version",
                source: e,
            })
        })?
        .ok_or_else(|| RollbackError::VersionNotFound {
            version: runner_ctx.version.clone(),
        })?;
    // 3a. Verify the ledger row belongs to the same logical app bucket
    // that owns the advisory lock. An operator-constructed or stale
    // MigrationPlan whose bucket.app differs from the row's
    // app_label would mutate the row while holding a lock for the
    // wrong bucket — the same hazard the repair flow guards
    // (GH #274 hardening).
    if row.app_label != plan.bucket.app {
        return Err(RollbackError::BucketAppMismatch {
            version: runner_ctx.version.clone(),
            row_app_label: row.app_label.clone(),
            supplied_app: plan.bucket.app.clone(),
        });
    }

    if !matches!(row.status, LedgerStatus::Applied | LedgerStatus::Faked) {
        return Err(RollbackError::VersionNotRollbackable {
            version: runner_ctx.version.clone(),
            current_status: row.status,
        });
    }

    // #355: The destructive rollback path must prove that the
    // current file-derived checksums match the applied ledger row
    // before any down SQL runs. This lives inside the advisory-lock
    // window so the status/checksum decision is atomic with the
    // later rollback mutation.
    validate_rollback_checksum_parity(&row, runner_ctx)?;

    // #366: Pre-strict leaf-identity check — zero-leaf↔non-empty drift.
    if let Some(ref stored_identity) = row.leaf_identity {
        let pre_cache = compute_leaf_identity_cache(ctx, plan)
            .await
            .map_err(RollbackError::runner)?;
        let pre_identity = serialize_leaf_identity(&pre_cache).unwrap_or_default();
        if pre_identity != *stored_identity {
            return Err(RollbackError::LeafIdentityMismatch {
                version: runner_ctx.version.clone(),
                stored_leaf_identity: stored_identity.clone(),
                current_leaf_identity: pre_identity,
            });
        }
    }

    // 5. Materialize strict replay plan inside advisory lock. Partition-expanded
    // statements are rematerialized from the same execution stream used by apply;
    // rollback replays reverse down SQL against the expanded stream. Lossy
    // scanning operates on the materialized plan, not the original unexpanded
    // plan, so per-leaf lossy markers are properly detected (GH #317).
    let (replay_plan, leaves_cache_rollback) =
        materialize_execution_plan(ctx, plan, PartitionExpansionMode::ReplayStrict)
            .await
            .map_err(RollbackError::runner)?;

    // #356: Compare stored leaf identity against freshly materialized leaves.
    let current_rollback_identity =
        serialize_leaf_identity(&leaves_cache_rollback).unwrap_or_default();
    let rollback_leaf_mismatch = if let Some(ref stored_identity) = row.leaf_identity {
        current_rollback_identity != *stored_identity
    } else {
        false
    };
    if rollback_leaf_mismatch {
        return Err(RollbackError::LeafIdentityMismatch {
            version: runner_ctx.version.clone(),
            stored_leaf_identity: row.leaf_identity.clone().unwrap(),
            current_leaf_identity: current_rollback_identity,
        });
    }

    // 5a. Lossy scanning against the materialized (expanded) plan.
    let allow_reason = rollback_lossy_allow_reason(&replay_plan, &lossy_policy)?;

    // 5b. Phase 0 down payload classification — refuse seed-capable runtime,
    // seed-DML non-runtime, generated-stale, ambiguous, incomplete, or missing
    // Phase 0 down SQL before any transactional/non-transactional down
    // execution. The helper consumes the already-materialized ReplayStrict
    // plan, so rollback checks the same authoritative stream it would execute.
    preflight_phase_zero_down_payload(&replay_plan, runner_ctx)?;

    if runner_ctx.version == super::bootstrap::PHASE_ZERO_VERSION {
        // Phase 0 is the run-id carve-out (HeeRanjID not yet installed),
        // so no identity binding is required for rollback of Phase 0.
    } else {
        // Non-Phase-0 rollback: enforce binding-capable identity before
        // down SQL execution. Refuse IdentityFree or missing identity
        // because non-Phase-0 rollback executes user migration SQL and
        // may cross HeeRanjID-backed run-id generation.
        let identity = match runner_ctx.runner_identity {
            Some(id) => id,
            None => {
                return Err(RollbackError::MissingRollbackIdentity {
                    version: runner_ctx.version.clone(),
                });
            }
        };

        if !identity.requires_binding() {
            // IdentityFree is not allowed for non-Phase-0 rollback.
            return Err(RollbackError::MissingRollbackIdentity {
                version: runner_ctx.version.clone(),
            });
        }

        // Bind the selected node identity on the pinned rollback
        // connection before down SQL execution.
        bind_runner_node_identity(
            ctx,
            identity.node_id().expect(
                "INVARIANT: node_id is Some when requires_binding is true; \
                 IdentityFree ruled out by refusal gate above",
            ),
        )
        .await
        .map_err(RollbackError::runner)?;
    }

    rollback_inner(ctx, &replay_plan, runner_ctx, prior_snapshot, allow_reason).await
}

/// Internal rollback core logic — split out so the advisory-lock release
/// runs on every exit branch.
/// **Atomicity contract.** The transactional segments share ONE
/// Postgres transaction, opened before the first segment's down SQL
/// runs and committed after the last. A failure mid-walk rolls back
/// the entire compound — no transactional segment commits in
/// isolation while a peer fails. Non-transactional segments are
/// inherently auto-committed and run before the compound transaction
/// (per the apply-order inversion the v3 plan specifies).
async fn rollback_inner(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    prior_snapshot: Option<&super::schema::AppliedSchema>,
    allow_reason: Option<String>,
) -> Result<RollbackReport, RollbackError> {
    // 5. Walk segments in reverse list order. Apply runs
    // transactional segments first then non-transactional. Rollback
    // inverts that:
    // a. Non-transactional segments first (auto-committed,
    // REVERSE statement order per segment, REVERSE segment
    // order across the plan).
    // b. Transactional segments second, ALL inside a SINGLE
    // compound Postgres transaction (REVERSE statement order
    // per segment, REVERSE segment order across the plan).
    // A failure inside the compound tx aborts the whole tx.
    let mut transactional_undone = 0usize;
    let mut non_transactional_undone = 0usize;

    // Phase a — non-transactional segments, in reverse plan order.
    for (rev_idx, segment) in plan.segments.iter().enumerate().rev() {
        if segment.kind == SegmentKind::NonTransactional {
            rollback_non_transactional_segment(ctx, segment, rev_idx, runner_ctx).await?;
            non_transactional_undone += 1;
        }
    }

    // Phase b — every transactional segment inside ONE Postgres
    // transaction. Open BEGIN once; walk segments in reverse plan
    // order, statements in reverse per segment; ROLLBACK on any
    // failure; COMMIT at the end.
    let has_transactional = plan
        .segments
        .iter()
        .any(|s| s.kind == SegmentKind::Transactional);
    if has_transactional {
        ctx.batch_execute("BEGIN")
            .await
            .map_err(|e| RollbackError::DownStatementFailed {
                segment_index: usize::MAX,
                statement_label: "<BEGIN compound rollback tx>".to_string(),
                // Phase-a non-tx segments auto-commit per statement. If any
                // ran, the live DB is already mutated even though the
                // compound-tx BEGIN never succeeded — report committed so the
                // caller rebuilds derived state instead of leaving it stale.
                live_db_committed: non_transactional_undone > 0,
                source: e,
            })?;

        for (rev_idx, segment) in plan.segments.iter().enumerate().rev() {
            if segment.kind != SegmentKind::Transactional {
                continue;
            }
            for stmt in segment.statements.iter().rev() {
                if stmt.down.is_empty() {
                    continue;
                }
                if let Err(e) = execute_runner_statement(ctx, &stmt.down, runner_ctx).await {
                    // Best-effort ROLLBACK of the whole compound tx
                    // surface the original error verbatim.
                    let _ = ctx.batch_execute("ROLLBACK").await;
                    return Err(RollbackError::DownStatementFailed {
                        segment_index: rev_idx,
                        statement_label: stmt.label.clone(),
                        // The compound tx is rolled back, so the transactional
                        // segments left no residue. But Phase-a non-tx segments
                        // already auto-committed; if any ran, the live DB is
                        // mutated and the caller must rebuild derived state.
                        live_db_committed: non_transactional_undone > 0,
                        source: e,
                    });
                }
            }
            transactional_undone += 1;
        }

        ctx.batch_execute("COMMIT")
            .await
            .map_err(|e| RollbackError::DownStatementFailed {
                segment_index: usize::MAX,
                statement_label: "<COMMIT compound rollback tx>".to_string(),
                live_db_committed: true,
                source: e,
            })?;
    }

    // 6. Update the ledger row to `rolled_back`. The note records the
    // rollback timestamp and (when applicable) the lossy reason.
    // clear `total_steps` to NULL so a rolled-back row no
    // longer advertises stale progress. The columns we touch are:
    // - status -> 'rolled_back'
    // - applied_steps_count -> 0
    // - total_steps -> NULL
    // - partial_apply_note -> rollback record
    let timestamp = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "<unknown timestamp>".to_string());
    let note = match allow_reason.as_deref() {
        Some(reason) => format!("rolled back at {timestamp}; lossy reason: {reason}"),
        None => format!("rolled back at {timestamp}"),
    };
    ctx.execute(
        "UPDATE djogi_schema_migrations \
         SET status = 'rolled_back', \
             applied_steps_count = 0, \
             total_steps = NULL, \
             partial_apply_note = $2 \
         WHERE version = $1 AND app_label = $3",
        &[&runner_ctx.version, &note, &runner_ctx.bucket.app],
    )
    .await
    .map_err(|e| {
        RollbackError::runner_committed(RunnerError::LedgerWriteFailed {
            version: runner_ctx.version.clone(),
            source: e,
        })
    })?;

    // 7. Persist the prior snapshot, if supplied. The caller maintains
    // snapshot history; the rollback path only writes whatever was handed in.
    // The `prior_snapshot.is_none() && snapshot_path.is_some()` case
    // is rejected at the TOP of `rollback_plan` — by the time
    // we reach this branch the invariant is "either both are present
    // or `prior_snapshot` is None and `snapshot_path` is also None".
    let mut snapshot_reverted = false;
    if let (Some(snap), Some(path)) = (prior_snapshot, &runner_ctx.snapshot_path) {
        save_snapshot(snap, path).map_err(|e| RollbackError::SnapshotPersistFailed {
            path: path.clone(),
            source: e,
        })?;
        snapshot_reverted = true;
    }

    Ok(RollbackReport {
        transactional_undone,
        non_transactional_undone,
        snapshot_reverted,
        lossy_reason: allow_reason,
    })
}

/// Run every `down` statement in a non-transactional segment, in
/// REVERSE statement order, with autocommit.
async fn rollback_non_transactional_segment(
    ctx: &mut DjogiContext,
    segment: &Segment,
    segment_index: usize,
    runner_ctx: &RunnerCtx,
) -> Result<(), RollbackError> {
    for stmt in segment.statements.iter().rev() {
        if stmt.down.is_empty() {
            continue;
        }
        if let Err(e) = execute_runner_statement(ctx, &stmt.down, runner_ctx).await {
            return Err(RollbackError::DownStatementFailed {
                segment_index,
                statement_label: stmt.label.clone(),
                live_db_committed: true,
                source: e,
            });
        }
    }
    Ok(())
}

/// Successful rollback report.
#[derive(Debug, Clone)]
pub struct RollbackReport {
    /// Number of transactional segments whose `down` SQL ran.
    pub transactional_undone: usize,
    /// Number of non-transactional segments whose `down` SQL ran.
    pub non_transactional_undone: usize,
    /// `true` when [`save_snapshot`] was invoked with the
    /// caller-supplied prior snapshot.
    pub snapshot_reverted: bool,
    /// Lossy-rollback reason (when the policy was `Allow`). `None`
    /// for clean rollbacks.
    pub lossy_reason: Option<String>,
}

/// Mark a migration version as `faked` — record the row in the ledger
/// without running any SQL.
/// **Use case.** An out-of-band tool already applied the schema
/// change (manual SQL, prior dev tooling, restored backup) and the
/// operator wants Djogi's ledger to reflect "this version is
/// considered applied; skip its DDL on a future apply".
/// **Snapshot moves forward.** The caller supplies a `snapshot` and
/// `snapshot_path` representing the state Djogi should consider
/// authoritative now. Fake-apply asserts that the schema is in this
/// state without verifying it — the operator owns that verification.
/// **Why a separate function.** Keeping fake-apply distinct from
/// `apply_plan` makes the audit trail honest: the ledger row carries
/// `status = 'faked'` (not `applied`) and a `partial_apply_note`
/// describing the operator's reason. Anyone reviewing the ledger
/// later can immediately see the row was not produced by the runner's
/// happy path.
/// `reason` is required and persisted to `partial_apply_note`.
pub async fn fake_apply_plan(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    _guard: &WorkspaceGuard,
    reason: &str,
) -> Result<RunReport, RunnerError> {
    preflight_phase_zero_apply_artifact(plan, runner_ctx)?;
    verify_plan_checksum(plan, runner_ctx)?;
    validate_apply_identity(runner_ctx)?;

    // GH #274 / #331 — pin one physical Postgres session (same contract as apply_plan).
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RunnerError::PinnedSessionCheckoutFailed { source: e })?;
    fake_apply_pinned(&mut pinned, plan, runner_ctx, reason).await
}

/// Internal fake-apply path that runs on an already-pinned context.
async fn fake_apply_pinned(
    ctx: &mut PinnedCtx<'_>,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    reason: &str,
) -> Result<RunReport, RunnerError> {
    // Same advisory-lock dance as apply_plan; fake-apply still needs
    // exclusive access to the ledger row insertion.
    ledger::bootstrap(ctx)
        .await
        .map_err(|e| RunnerError::LedgerBootstrapFailed { source: e })?;

    let lock_key = advisory_lock_key(&plan.bucket);
    acquire_advisory_lock(ctx, &plan.bucket, lock_key).await?;

    let result = fake_apply_inner(ctx, plan, runner_ctx, reason).await;
    let released = release_advisory_lock(ctx, lock_key).await;
    let result = handle_release_result(result, released, &plan.bucket, lock_key);
    if result.is_ok() {
        ctx.mark_clean();
    }
    result
}

async fn fake_apply_inner(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    runner_ctx: &RunnerCtx,
    reason: &str,
) -> Result<RunReport, RunnerError> {
    let started = Instant::now();

    // Verify checksum still matches the plan — fake-apply does not
    // run SQL but it still records `checksum_up`, and the row is more
    // useful when the recorded checksum reflects the actual plan.
    let computed_up = compute_checksum_for_plan_up(plan);
    if let Err(e) =
        ledger::verify_checksum(&runner_ctx.version, &runner_ctx.checksum_up, &computed_up)
    {
        return Err(match e {
            VerifyError::Mismatch(m) => RunnerError::ChecksumMismatch(m),
            VerifyError::Format(f) => RunnerError::ChecksumFormat(f),
        });
    }

    // Out-of-order detection — fake-apply respects the same policy
    // gate as real apply. A faked row with a suppressed out_of_order_flag
    // would misrepresent the version-ordering state in the ledger.
    let conflicting_peer = find_higher_applied_version(ctx, &plan.bucket, &runner_ctx.version)
        .await
        .map_err(|e| RunnerError::LedgerQueryFailed {
            query_label: "out_of_order_check",
            source: e,
        })?;
    let is_out_of_order = conflicting_peer.is_some();
    if is_out_of_order && !runner_ctx.out_of_order_policy.allows() {
        let (conflicting_version, conflicting_applied_at) =
            conflicting_peer.unwrap_or_else(|| (String::new(), None));
        return Err(RunnerError::OutOfOrderRejected {
            version: runner_ctx.version.clone(),
            conflicting_version,
            conflicting_applied_at,
        });
    }
    if is_out_of_order {
        let (conflicting_version, applied_at) = conflicting_peer
            .as_ref()
            .map(|(v, ts)| (v.as_str(), ts.as_deref()))
            .unwrap_or(("", None));
        tracing::warn!(
            bucket_database = %plan.bucket.database,
            bucket_app = %plan.bucket.app,
            version = %runner_ctx.version,
            conflicting_version,
            conflicting_applied_at = applied_at.unwrap_or("<unknown>"),
            policy = ?runner_ctx.out_of_order_policy,
            "out-of-order fake-apply allowed by policy",
        );
    }

    let timestamp = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "<unknown timestamp>".to_string());
    let fake_note = format!("faked at {timestamp}; reason: {reason}");
    // Compose the out-of-order portion using the same function as
    // apply_plan_inner. The fake-apply reason comes first (primary
    // operation); the out-of-order annotation is supplementary context.
    let ooo_note = compose_initial_note(
        is_out_of_order,
        runner_ctx.out_of_order_policy.override_reason(),
        conflicting_peer.as_ref(),
    );
    let note = match ooo_note {
        Some(note_str) => format!("{fake_note}; {note_str}"),
        None => fake_note,
    };

    // Gate: non-Phase-0 fake-apply requires a binding-capable runner
    // identity before `generate_run_id()`. Mirrors the apply_gate
    // and rollback inverse. Phase 0 remains exempt.
    if runner_ctx.version != super::bootstrap::PHASE_ZERO_VERSION {
        let identity = match runner_ctx.runner_identity {
            Some(id) => id,
            None => {
                return Err(RunnerError::MissingApplyIdentity {
                    version: runner_ctx.version.clone(),
                });
            }
        };
        if !identity.requires_binding() {
            return Err(RunnerError::MissingApplyIdentity {
                version: runner_ctx.version.clone(),
            });
        }

        // Bind the selected node identity on the pinned connection.
        bind_runner_node_identity(
            ctx,
            identity.node_id().expect(
                "INVARIANT: node_id is Some when requires_binding is true; \
                 IdentityFree ruled out by refusal gate above",
            ),
        )
        .await?;
    }

    let run_id = generate_run_id(ctx, &runner_ctx.version).await?;
    // `insert_pending` binds `row.status.as_db_str()` directly, so
    // constructing the row with `LedgerStatus::Faked` writes the
    // correct terminal status in a single INSERT — no post-insert
    // UPDATE needed (cluster-2 simplify Finding 1). The concern
    // (crash between INSERT and UPDATE leaving a stranded `pending`
    // row) is eliminated because there is now only one DB operation.
    let row = LedgerRow {
        version: runner_ctx.version.clone(),
        description: runner_ctx.description.clone(),
        checksum_up: runner_ctx.checksum_up.clone(),
        checksum_down: runner_ctx.checksum_down.clone(),
        execution_mode: ExecutionMode::Transactional,
        status: LedgerStatus::Faked,
        execution_time_ms: 0,
        out_of_order_flag: is_out_of_order,
        applied_steps_count: 0,
        total_steps: None,
        partial_apply_note: Some(note.clone()),
        run_id,
        snapshot_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        app_label: plan.bucket.app.clone(),
        leaf_identity: None,
    };

    let ledger_id = match ledger::insert_pending(ctx, &row).await {
        Ok(id) => id,
        Err(e) => {
            if is_unique_violation(&e) {
                return Err(classify_duplicate_version_collision(
                    ctx,
                    &runner_ctx.version,
                    &runner_ctx.bucket.app,
                )
                .await);
            }
            return Err(RunnerError::LedgerWriteFailed {
                version: runner_ctx.version.clone(),
                source: e,
            });
        }
    };

    // Snapshot moves forward when supplied. The write ordering is:
    // a. W1: ledger row committed as terminal `faked` (already done above).
    // b. W3: persist snapshot to disk.
    // If (b) fails, the ledger row is `faked` but the snapshot is stale or
    // missing. Recovery: run `djogi migrations compose` (or `attune`) to
    // regenerate the snapshot from the descriptor inventory. The ledger row
    // does not need repair — it correctly records that the migration was
    // faked. See #326 Amendment 3 (W3a/W3b/W3c crash states).
    if let (Some(snapshot), Some(path)) = (&runner_ctx.snapshot, &runner_ctx.snapshot_path) {
        save_snapshot(snapshot, path).map_err(|e| RunnerError::SnapshotPersistFailed {
            path: path.clone(),
            source: e,
        })?;
    }

    let elapsed = elapsed_ms(started);
    Ok(RunReport {
        ledger_id,
        run_id,
        transactional_segments: 0,
        non_transactional_segments: 0,
        metadata_segments: 0,
        execution_time_ms: elapsed,
    })
}

/// Establish a baseline ledger row for an existing database that was
/// created without Djogi (or by an earlier Djogi version).
/// **What it does.** Projects the LIVE database catalog into
/// an [`AppliedSchema`] using the verify-side projection helper, then
/// inserts a single ledger row with `status = 'baseline'`,
/// `checksum_up` derived from the projected schema, and a
/// `description` that includes a `<baseline>` marker. No SQL runs
/// against user tables; the schema is whatever Postgres currently
/// holds, captured exactly. The projection is then persisted to
/// `runner_ctx.snapshot_path` (when provided) as the canonical
/// baseline so future migrations diff against it.
/// **The runner DOES NOT trust a caller-supplied snapshot.**
/// review flagged that the previous arrangement let an
/// operator baseline an existing schema with a stale snapshot
/// future diffs then started from the wrong state. To prevent that
/// failure mode, the runner now refuses any caller that pre-fills
/// `runner_ctx.snapshot` and instead always projects fresh.
/// **One baseline per bucket.** A bucket should carry at most one
/// `baseline` row in its history. The unique-violation on `version`
/// already enforces this when the operator picks the convention
/// (e.g. `V0__baseline`); the runner does not enforce one-per-bucket
/// itself because the ledger is shared across buckets via `app_label`
/// and the operator owns the version-naming policy.
/// `reason` is recorded in `partial_apply_note` so the audit trail
/// captures why the baseline was established.
pub async fn baseline_plan(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
    runner_ctx: &RunnerCtx,
    _guard: &WorkspaceGuard,
    reason: &str,
) -> Result<RunReport, RunnerError> {
    // Refuse caller-supplied snapshots.
    if runner_ctx.snapshot.is_some() {
        return Err(RunnerError::BaselineSnapshotShouldNotBeProvided);
    }
    validate_apply_identity(runner_ctx)?;

    // GH #274 / #331 — pin one physical Postgres session (same contract as apply_plan).
    let mut pinned = ctx
        .pin_for_migration()
        .await
        .map_err(|e| RunnerError::PinnedSessionCheckoutFailed { source: e })?;
    baseline_pinned(&mut pinned, bucket, runner_ctx, reason).await
}

/// Internal baseline path that runs on an already-pinned context.
async fn baseline_pinned(
    ctx: &mut PinnedCtx<'_>,
    bucket: &BucketKey,
    runner_ctx: &RunnerCtx,
    reason: &str,
) -> Result<RunReport, RunnerError> {
    ledger::bootstrap(ctx)
        .await
        .map_err(|e| RunnerError::LedgerBootstrapFailed { source: e })?;

    let lock_key = advisory_lock_key(bucket);
    acquire_advisory_lock(ctx, bucket, lock_key).await?;

    let result = baseline_inner(ctx, bucket, runner_ctx, reason).await;
    let released = release_advisory_lock(ctx, lock_key).await;
    let result = handle_release_result(result, released, bucket, lock_key);
    if result.is_ok() {
        ctx.mark_clean();
    }
    result
}

async fn baseline_inner(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
    runner_ctx: &RunnerCtx,
    reason: &str,
) -> Result<RunReport, RunnerError> {
    let started = Instant::now();

    // Project the live DB into an AppliedSchema. The projection
    // helper is reserved for repair / baseline use — verify uses the
    // same machinery internally. Failures here surface as the typed
    // BaselineProjectionFailed variant so the operator sees that the
    // projection (not the ledger) was the failing step.
    // Bucket-scoped (
    // tables that match this bucket's app boundary, so an app's
    // baseline does not capture another app's tables.
    // `None` snapshot fallback (#370): baseline projects the live DB to
    // establish the snapshot, so it has no on-disk snapshot to scope from
    // inventory-driven scoping only, behavior unchanged.
    let projected = super::verify::live_schema_for_repair(ctx, bucket, None)
        .await
        .map_err(|e| RunnerError::BaselineProjectionFailed {
            source: Box::new(e),
        })?;

    // Compute `checksum_up` over a deterministic rendering of the
    // projected schema. Baseline rows do not carry SQL fragments to
    // hash, so we hash the JSON serialization of the projection
    // itself — a content-addressed marker the operator can later
    // re-derive from the same DB state.
    let checksum_up = checksum_for_baseline_snapshot(&projected);

    let timestamp = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "<unknown timestamp>".to_string());
    let note = format!(
        "baseline established at {timestamp} for bucket database={db} app={app}; reason: {reason}",
        db = bucket.database,
        app = bucket.app,
    );

    // Gate: non-Phase-0 baseline requires a binding-capable runner
    // identity before `generate_run_id()`. Mirrors the apply_gate
    // and rollback inverse. Phase 0 remains exempt.
    if runner_ctx.version != super::bootstrap::PHASE_ZERO_VERSION {
        let identity = match runner_ctx.runner_identity {
            Some(id) => id,
            None => {
                return Err(RunnerError::MissingApplyIdentity {
                    version: runner_ctx.version.clone(),
                });
            }
        };
        if !identity.requires_binding() {
            return Err(RunnerError::MissingApplyIdentity {
                version: runner_ctx.version.clone(),
            });
        }

        // Bind the selected node identity on the pinned connection.
        bind_runner_node_identity(
            ctx,
            identity.node_id().expect(
                "INVARIANT: node_id is Some when requires_binding is true; \
                 IdentityFree ruled out by refusal gate above",
            ),
        )
        .await?;
    }

    let run_id = generate_run_id(ctx, &runner_ctx.version).await?;
    let row = LedgerRow {
        version: runner_ctx.version.clone(),
        description: format!("<baseline> {}", runner_ctx.description),
        checksum_up: checksum_up.clone(),
        checksum_down: None,
        execution_mode: ExecutionMode::Transactional,
        status: LedgerStatus::Baseline,
        execution_time_ms: 0,
        out_of_order_flag: false,
        applied_steps_count: 0,
        total_steps: None,
        partial_apply_note: Some(note),
        run_id,
        snapshot_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        app_label: bucket.app.clone(),
        leaf_identity: None,
    };
    // `insert_pending` binds `row.status.as_db_str()` directly, so
    // constructing the row with `LedgerStatus::Baseline` writes the
    // correct terminal status in a single INSERT — no post-insert
    // UPDATE needed (cluster-2 simplify Finding 1).
    let ledger_id = match ledger::insert_pending(ctx, &row).await {
        Ok(id) => id,
        Err(e) => {
            if is_unique_violation(&e) {
                return Err(classify_duplicate_version_collision(
                    ctx,
                    &runner_ctx.version,
                    &runner_ctx.bucket.app,
                )
                .await);
            }
            return Err(RunnerError::LedgerWriteFailed {
                version: runner_ctx.version.clone(),
                source: e,
            });
        }
    };

    // Persist the projected schema as the canonical baseline snapshot
    // when a path was supplied. (Tests that only care about ledger
    // semantics pass `snapshot_path: None`.)
    if let Some(path) = &runner_ctx.snapshot_path {
        save_snapshot(&projected, path).map_err(|e| RunnerError::SnapshotPersistFailed {
            path: path.clone(),
            source: e,
        })?;
    }

    let elapsed = elapsed_ms(started);
    Ok(RunReport {
        ledger_id,
        run_id,
        transactional_segments: 0,
        non_transactional_segments: 0,
        metadata_segments: 0,
        execution_time_ms: elapsed,
    })
}

/// Compute a `V1:<sha256-hex>` checksum over the canonical JSON
/// rendering of an [`AppliedSchema`]. Baseline rows hash the
/// projection itself (no SQL fragments exist for a baseline) so the
/// stored checksum is content-addressed: re-projecting the same DB
/// later must yield the same checksum. Used by `baseline_plan`
/// and `repair_snapshot_rebuild`.
pub(crate) fn checksum_for_baseline_snapshot(schema: &super::schema::AppliedSchema) -> String {
    // serde_json with sorted keys gives a deterministic byte stream;
    // BTreeMap fields in AppliedSchema already serialize alphabetically.
    // Failure here is impossible in practice (in-memory schema -> JSON)
    // but we degrade gracefully to an empty-input checksum if it ever
    // fires so the function stays total.
    let json = serde_json::to_string(schema).unwrap_or_default();
    compute_checksum([json])
}

/// Read a ledger row for a given version. Used by the rollback path
/// to confirm the row is in a state that admits rollback.
/// Delegates to [`ledger::load_full_row_by_version`] — the 15-column
/// SELECT and try_get cascade live in ledger.rs to avoid triplication
/// across runner / repair / verify.
async fn load_ledger_row_for_version(
    ctx: &mut DjogiContext,
    version: &str,
    app_label: &str,
) -> Result<Option<LedgerRow>, DjogiError> {
    load_full_row_by_version(ctx, version, app_label).await
}

// ── Segment dispatch helpers ──────────────────────────────────────────────

async fn execute_runner_statement(
    ctx: &mut DjogiContext,
    sql: &str,
    runner_ctx: &RunnerCtx,
) -> Result<(), DjogiError> {
    // The generated phase-zero bootstrap seeds Djogi-owned session
    // GUCs before HeerId exists. Keep the carve-out bound to that
    // canonical framework migration; adopter migrations still route
    // through the session-statement guard.
    if runner_ctx.version == super::bootstrap::PHASE_ZERO_VERSION {
        // Deepest statement-level guard: classify Phase 0 payloads
        // immediately before raw batch_execute. The pre-bootstrap
        // artifact checks are the primary gate, but this catches
        // stale statements that somehow evaded the artifact-level
        // classification (e.g. hand-edited migration files or
        // partial regeneration). Refuses top-level seed-DML statements and
        // generated-stale patterns like
        // ALTER DATABASE "hardcoded_db" SET heer.node_id.
        if let Err(reason) = super::phase_zero::require_current_phase_zero_statement(sql) {
            return Err(DjogiError::StalePhaseZeroStatement {
                refusal_reason: reason,
                statement: truncate_for_log(sql),
            });
        }
        ctx.batch_execute(sql).await
    } else {
        guarded_batch_execute(ctx, sql).await
    }
}

/// Truncate a SQL statement for safe inclusion in error messages.
/// Keeps the first 256 characters (enough to identify the pattern)
/// and appends an ellipsis if truncated.
fn truncate_for_log(sql: &str) -> String {
    const MAX_LEN: usize = 256;
    let trimmed = sql.trim();
    if trimmed.len() <= MAX_LEN {
        trimmed.to_string()
    } else {
        let mut end = 0;
        for (idx, ch) in trimmed.char_indices() {
            let next = idx + ch.len_utf8();
            if next > MAX_LEN {
                break;
            }
            end = next;
        }
        format!("{}...", &trimmed[..end])
    }
}

/// Run every statement inside a transactional segment within a
/// single Postgres transaction. On any error, ROLLBACK and surface
/// the failing statement label.
/// `segment_index` is the caller's loop index — threaded in so the
/// error variant carries the correct position rather than always
/// reporting `0` (cluster-2 simplify Finding 2).
async fn run_transactional_segment(
    ctx: &mut DjogiContext,
    segment: &Segment,
    segment_index: usize,
    runner_ctx: &RunnerCtx,
    add_table_set: &BTreeSet<String>,
) -> Result<(), RunnerError> {
    // PK-flip verification segment short-circuit: when every statement
    // in this segment carries a `PkFlipVerify` label the segment is
    // a halt-point gate, not DDL. Run each as a `query_one` against
    // a `SELECT count(*)` body and assert the count is zero.
    // No BEGIN/COMMIT — the queries are read-only.
    let all_verify = !segment.statements.is_empty()
        && segment
            .statements
            .iter()
            .all(|s| s.label.starts_with("PkFlipVerify "));
    if all_verify {
        for stmt in &segment.statements {
            // Recover the table from the label format "PkFlipVerify
            // <table> <hint>". `<table>` is the second whitespace-
            // separated token.
            let table = stmt
                .label
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            let row = ctx.query_one(&stmt.up, &[]).await.map_err(|e| {
                RunnerError::TransactionalSegmentFailed {
                    segment_index,
                    statement_label: stmt.label.clone(),
                    source: e,
                }
            })?;
            let count: i64 = row.try_get(0).unwrap_or(0);
            if count > 0 {
                return Err(RunnerError::PkFlipVerificationFailed {
                    table,
                    count_violating: count,
                });
            }
        }
        return Ok(());
    }

    // Probe relpages for any AddIndex statement that does NOT
    // require out-of-transaction. The probe runs BEFORE BEGIN so
    // the abort path on `strict_concurrent_warnings` doesn't leave
    // an open transaction around.
    for stmt in &segment.statements {
        if let Some((index_name, target_table)) = parse_create_index_statement(stmt) {
            relpages_probe(ctx, runner_ctx, &index_name, &target_table, add_table_set).await?;
        }
    }

    ctx.batch_execute("BEGIN")
        .await
        .map_err(|e| RunnerError::TransactionalSegmentFailed {
            segment_index,
            statement_label: "<BEGIN>".to_string(),
            source: e,
        })?;

    for stmt in &segment.statements {
        if let Err(e) = execute_runner_statement(ctx, &stmt.up, runner_ctx).await {
            // Best-effort rollback — surface the original error
            // regardless of whether the rollback succeeds.
            let _ = ctx.batch_execute("ROLLBACK").await;
            return Err(RunnerError::TransactionalSegmentFailed {
                segment_index,
                statement_label: stmt.label.clone(),
                source: e,
            });
        }
    }

    ctx.batch_execute("COMMIT")
        .await
        .map_err(|e| RunnerError::TransactionalSegmentFailed {
            segment_index,
            statement_label: "<COMMIT>".to_string(),
            source: e,
        })?;

    Ok(())
}

/// Run every statement in a non-transactional segment with autocommit.
/// Before each step, durably claim the boundary in
/// `partial_apply_note`; after the SQL commits, durably acknowledge
/// the new `applied_steps_count`. If the post-DDL ack fails, the
/// claim note remains in place so repair resume refuses to re-run the
/// ambiguous step automatically.
/// Returns the number of steps completed within this segment so the
/// outer runner can update its running tally of cross-segment progress.
struct NonTransactionalSegmentRun<'a> {
    segment_index: usize,
    version: &'a str,
    ledger_id: i64,
    prior_steps_completed: i32,
    total_non_tx_steps: i32,
    stable_note: Option<&'a str>,
    runner_ctx: &'a RunnerCtx,
}

async fn run_non_transactional_segment(
    ctx: &mut DjogiContext,
    segment: &Segment,
    run: NonTransactionalSegmentRun<'_>,
) -> Result<i32, RunnerError> {
    let mut completed: i32 = 0;
    for (step_idx, stmt) in segment.statements.iter().enumerate() {
        let claimed_step = run
            .prior_steps_completed
            .saturating_add(completed)
            .saturating_add(1);
        let claim_note = ledger::format_non_tx_progress_claim(
            run.stable_note,
            claimed_step,
            Some(run.total_non_tx_steps),
            run.segment_index,
            &stmt.label,
        );
        ledger::claim_non_tx_progress(ctx, run.ledger_id, &claim_note)
            .await
            .map_err(|e| RunnerError::LedgerWriteFailed {
                version: run.version.to_string(),
                source: e,
            })?;
        if let Err(e) = execute_runner_statement(ctx, &stmt.up, run.runner_ctx).await {
            let total_so_far = run.prior_steps_completed.saturating_add(completed);
            let note = format!(
                "non-tx step {step} of segment {seg} failed: {label} — {e}",
                step = step_idx + 1,
                seg = run.segment_index,
                label = stmt.label,
            );
            // Best-effort partial-state record. If the ledger update
            // itself fails, we still surface the original step
            // failure — the partial-state record is forensic only.
            let _ = ledger::mark_partial(ctx, run.ledger_id, total_so_far, &note).await;
            return Err(RunnerError::NonTransactionalSegmentFailed {
                segment_index: run.segment_index,
                step_index: step_idx,
                statement_label: stmt.label.clone(),
                applied_steps_count: total_so_far,
                source: e,
            });
        }
        completed = completed.saturating_add(1);
        let total_so_far = run.prior_steps_completed.saturating_add(completed);
        ledger::ack_non_tx_progress(ctx, run.ledger_id, total_so_far, run.stable_note)
            .await
            .map_err(|e| RunnerError::NonTransactionalProgressAckFailed {
                segment_index: run.segment_index,
                step_index: step_idx,
                statement_label: stmt.label.clone(),
                applied_steps_count: total_so_far,
                source: e,
            })?;
    }
    Ok(completed)
}

// ── Advisory lock + relpages probe ────────────────────────────────────────

/// Derive a stable 64-bit advisory-lock key from a `BucketKey`. Uses
/// SHA-256 truncated to the low 64 bits for a fixed-seed hash
/// stdlib `Hasher` is randomised per process and cannot be used.
/// **Byte order: big-endian.** The v3 contract pins
/// big-endian decoding of the first 8 SHA-256 digest bytes so an
/// alternate-language implementation following the same spec
/// computes the identical `i64` and contends correctly with this
/// runner. Network byte order is the spec; little-endian would key
/// the same bucket to a different value across implementations.
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

/// Acquire a Postgres advisory lock on `key`.
/// Postgres `pg_advisory_lock(bigint)` blocks indefinitely; this
/// function uses `pg_try_advisory_lock(bigint)` in a bounded retry
/// loop so a stuck holder cannot wedge the runner.
/// **Session contract (GH #274).** This function MUST be called on
/// a pinned `DjogiContext` — i.e., one backed by a single checked-out
/// `PgConnection`. Postgres session-level advisory locks are bound to
/// the physical backend that issued the SQL; calling this on a
/// pool-backed context would acquire the lock on connection A, but
/// subsequent DDL/ledger operations would run on B, C, … and the
/// `release_advisory_lock` call would return false when invoked on any
/// connection other than A. Runner entry points (`apply_plan`,
/// `rollback_plan`, `fake_apply_plan`, `baseline_plan`) and repair
/// entry points in `migrate::repair` enforce this by pinning a
/// connection from the pool before calling here.
pub(crate) async fn acquire_advisory_lock(
    ctx: &mut PinnedCtx<'_>,
    bucket: &BucketKey,
    key: i64,
) -> Result<(), RunnerError> {
    // GH #331 [H-331-1] — hard assert in ALL build profiles.
    assert!(
        !ctx.is_pool_backed(),
        "acquire_advisory_lock called on a pool-backed context — \
         the advisory lock would be acquired on an arbitrary pool \
         connection and subsequent operations would run on different \
         connections. Callers must use ctx.pin_for_migration() first \
         (GH #274 / #331).",
    );
    const MAX_ATTEMPTS: u32 = 600; // 600 * 50 ms = 30 s
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    for attempt in 0..MAX_ATTEMPTS {
        let row = ctx
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .await
            .map_err(|e| RunnerError::AdvisoryLockQueryFailed {
                app_label: bucket.app.clone(),
                source: e,
            })?;
        let acquired: bool = row
            .try_get(0)
            .map_err(|e| RunnerError::AdvisoryLockQueryFailed {
                app_label: bucket.app.clone(),
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

/// Release a previously-acquired advisory lock.
/// Returns `true` when `pg_advisory_unlock` confirms the lock was
/// held and released. Returns `false` when `pg_advisory_unlock`
/// returns `false` — meaning the lock was NOT held on this physical
/// session. A `false` return is a session-pinning correctness failure
/// (GH #274 / #280): the lock was either acquired on a different
/// connection or never acquired at all.
/// When the unlock query itself fails (e.g. the connection closed),
/// this function logs a `tracing::warn!` and returns `true` — the
/// session death will release the lock anyway and the warn is the
/// observable signal.
/// **Callers must always invoke this function even when the migration
/// operation errored.** Use [`handle_release_result`] to reconcile
/// the operation result with the bool returned here: if the operation
/// succeeded but this returns `false`, surface
/// [`RunnerError::AdvisoryUnlockReturnedFalse`]; if both errored,
/// log this result and return the original error.
pub(crate) async fn release_advisory_lock(ctx: &mut PinnedCtx<'_>, key: i64) -> bool {
    assert!(
        !ctx.is_pool_backed(),
        "release_advisory_lock called on a pool-backed context — \
         the unlock would run on a different connection than the one \
         that holds the lock. Callers must use ctx.pin_for_migration() \
         first (GH #274 / #331).",
    );
    let row = match ctx
        .query_one("SELECT pg_advisory_unlock($1)", &[&key])
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // Query failure likely means the connection died; Postgres will
            // auto-release the lock when the backend exits.
            tracing::warn!(
                ?e,
                key,
                "pg_advisory_unlock query failed; lock will auto-release on session close",
            );
            return true;
        }
    };
    let released: bool = match row.try_get(0) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                ?e,
                key,
                "pg_advisory_unlock result could not be decoded; assuming released",
            );
            return true;
        }
    };
    if !released {
        // This is a session-pinning correctness failure: pg_advisory_unlock
        // returned false, meaning the lock was NOT held on this session.
        // Log at error level so it is never silently swallowed. Callers are
        // responsible for converting this to a typed RunnerError or RepairError.
        tracing::error!(
            key,
            "pg_advisory_unlock returned false — lock was not held on this Postgres \
             session. This indicates a session-pinning bug: the advisory lock was \
             acquired on a different physical backend than the one executing the \
             migration operations (GH #274/#280).",
        );
    }
    released
}

/// Reconcile an operation result with the advisory lock release outcome.
/// # Behaviour matrix
/// | Operation | Release | Result |
/// |-----------|---------|--------|
/// | `Ok` | `true` | `Ok` (success, lock properly released) |
/// | `Ok` | `false` | `Err(AdvisoryUnlockReturnedFalse)` — correctness failure |
/// | `Err` | `true` | `Err` — original operation error |
/// | `Err` | `false` | `Err` — original error; release failure already logged |
/// The `false` case on the success path is the hard correctness failure
/// added by GH #274 / #280: it means the migration SQL may have run but
/// the advisory lock was not protecting it on the correct session.
// RunnerError is a large enum by design (it carries full operator context in
// every variant). Async callers return it through boxed futures so clippy
// does not flag them; this sync helper also returns it and needs the
// suppression. Same rationale as reset.rs / compose.rs.
#[allow(clippy::result_large_err)]
pub(crate) fn handle_release_result<T>(
    result: Result<T, RunnerError>,
    released: bool,
    bucket: &BucketKey,
    key: i64,
) -> Result<T, RunnerError> {
    match result {
        Ok(v) if released => Ok(v),
        Ok(_) => Err(RunnerError::AdvisoryUnlockReturnedFalse {
            key,
            bucket: bucket.clone(),
        }),
        Err(e) => Err(e), // release false already logged inside release_advisory_lock
    }
}

/// Run the relpages probe before a transactional `CREATE INDEX`.
/// On WARN path: emit `tracing::warn!` and continue. On strict path
/// (`migrate.strict_concurrent_warnings = true`): surface
/// `RunnerError::RelpagesThresholdExceeded`.
/// **`pg_class.relpages = None` disambiguation.** A `None` row from
/// the probe means Postgres does not currently know about the table.
/// Two legitimate cases produce that:
/// 1. The current plan creates the table in an earlier segment of
///    THIS run (`AddTable` statement). The runner treats this as
///    `relpages = 0` (a freshly-created empty table cannot exceed
///    the threshold).
/// 2. The table genuinely does not exist and is not being created.
///    That is a typo / mis-quoted identifier — the index would fail
///    inside `BEGIN` anyway, but the strict-mode probe catches it
///    earlier with a clearer `TargetTableNotFound` diagnostic.
///    Silently dropping case 2 to `relpages = 0` would disable the
///    strict warning path for any mis-targeted `CREATE INDEX`, which
///    is exactly the failure mode strict-mode exists to catch.
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
        .map_err(|e| RunnerError::CatalogQueryFailed {
            query_label: "pg_class relpages",
            source: e,
        })?;
    let relpages: i32 = match row_opt {
        Some(r) => r
            .try_get::<_, i32>(0)
            .map_err(|e| RunnerError::CatalogQueryFailed {
                query_label: "pg_class relpages",
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

/// **PK-flip pre-flight gate** — run before any side effect when the
/// plan is a PK-type-flip migration.
/// Implements D060–D063 from the v3 plan contract:
/// - **D060** Logical-replication machinery active — `pg_stat_replication`
///   walsenders OR enabled rows in `pg_subscription` for the current
///   database → refusal. Postgres does not expose another backend's
///   `session_replication_role` GUC via `pg_stat_activity`, so we
///   detect the apply machinery itself rather than the GUC value.
/// - **D061** Pre-existing `zzz_*` triggers on the migrating tables
///   → refusal (collision with the autofill install).
/// - **D062** Already-disabled triggers on the migrating tables
///   → refusal.
/// - **D063** Open transactions older than the configured threshold
///   → refusal.
///   The set of "migrating tables" is recovered from the cutover
///   segment's labels — every `PkFlipPrep` / `PkFlipCutover` /
///   `PkFlipBackfill` / `PkFlipConcurrentIndex` / `PkFlipNotNullProof`
///   label carries the parent table name as its trailing token; we
///   also collect every child table name from the multi-segment SQL
///   via byte-level identifier scanning of the `up` text. This avoids
///   re-walking the descriptor here — the segment plan already carries
///   the full set the cutover will touch.
///   **No regex** — the table-name extraction uses byte-level forward
///   scans for `ALTER TABLE <ident> ` literal substrings; the
///   identifier rules (ASCII letter or underscore, then alphanumerics
///   or underscore, ≤ 63 bytes) are spelled out in plain English in
///   the helper.
async fn pk_flip_preflight(
    ctx: &mut DjogiContext,
    runner_ctx: &RunnerCtx,
    plan: &MigrationPlan,
) -> Result<(), RunnerError> {
    // Recover migrating tables from the segment plan. The label
    // format is `PkFlip<Stage> <parent>` (no spaces in the parent
    // identifier per descriptor / projection enforcement).
    let mut tables: BTreeSet<String> = BTreeSet::new();
    for seg in &plan.segments {
        for stmt in &seg.statements {
            if let Some(parent) = stmt.label.strip_prefix("PkFlipPrep ") {
                tables.insert(parent.to_string());
            } else if let Some(parent) = stmt.label.strip_prefix("PkFlipCutover ") {
                tables.insert(parent.to_string());
            } else if let Some(parent) = stmt.label.strip_prefix("PkFlipPartitionedPrep ") {
                tables.insert(parent.to_string());
            }
            // Scan `ALTER TABLE <ident>` to recover child tables.
            for child in scan_alter_table_targets(&stmt.up) {
                tables.insert(child);
            }
        }
    }

    // D060 — logical-replication machinery active in this DB.
    // Why the indirection: Postgres does not let one backend read
    // another backend's `session_replication_role` GUC from
    // `pg_stat_activity`. The playbook §12.3 hazard names "Logical
    // replication apply workers" as the concrete failure mode, and
    // those are observable via:
    // - `pg_stat_replication` — every active walsender (the apply
    // side runs with role = 'replica' by default).
    // - `pg_subscription` — subscriptions with `subenabled =
    // true` indicate an apply worker may be running in this DB.
    // We surface BOTH as the D060 hazard so the operator knows which
    // signal fired. Either alone is sufficient to refuse.
    let walsender_rows = ctx
        .query_all(
            "SELECT COALESCE(application_name, ''), COALESCE(client_addr::text, '') \
             FROM pg_stat_replication",
            &[],
        )
        .await
        .map_err(|e| RunnerError::CatalogQueryFailed {
            query_label: "pg_stat_replication",
            source: e,
        })?;
    let mut walsenders: Vec<(String, String)> = Vec::with_capacity(walsender_rows.len());
    for r in &walsender_rows {
        let app: String = r.try_get(0).unwrap_or_default();
        let client: String = r.try_get(1).unwrap_or_default();
        walsenders.push((app, client));
    }

    let sub_rows = ctx
        .query_all(
            "SELECT subname FROM pg_subscription \
             WHERE subdbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
               AND subenabled = true",
            &[],
        )
        .await
        .map_err(|e| RunnerError::CatalogQueryFailed {
            query_label: "pg_subscription",
            source: e,
        })?;
    let mut subscriptions: Vec<String> = Vec::with_capacity(sub_rows.len());
    for r in &sub_rows {
        let name: String = r.try_get(0).unwrap_or_default();
        subscriptions.push(name);
    }

    if !walsenders.is_empty() || !subscriptions.is_empty() {
        return Err(RunnerError::PkFlipHazardReplicaSessions {
            walsenders,
            subscriptions,
        });
    }

    // D061 + D062 — per-table trigger checks.
    for table in &tables {
        // The LIKE pattern `zzz\_%` uses `\` as an explicit escape
        // character (NOT regex) so the `_` is a literal underscore
        // rather than the LIKE wildcard.
        let zzz_rows = ctx
            .query_all(
                "SELECT tgname FROM pg_trigger \
                 WHERE tgrelid = (SELECT oid FROM pg_class WHERE relname = $1 AND relkind = 'r' LIMIT 1) \
                   AND NOT tgisinternal \
                   AND tgname LIKE 'zzz\\_%' ESCAPE '\\'",
                &[table],
            )
            .await
            .map_err(|e| RunnerError::CatalogQueryFailed {
                query_label: "pg_trigger zzz scan",
                source: e,
            })?;
        if !zzz_rows.is_empty() {
            let names: Vec<String> = zzz_rows
                .iter()
                .map(|r| r.try_get::<_, String>(0).unwrap_or_default())
                .collect();
            return Err(RunnerError::PkFlipHazardPreexistingZzzTrigger {
                table: table.clone(),
                trigger_names: names,
            });
        }
        let disabled_rows = ctx
            .query_all(
                "SELECT tgname, tgenabled FROM pg_trigger \
                 WHERE tgrelid = (SELECT oid FROM pg_class WHERE relname = $1 AND relkind = 'r' LIMIT 1) \
                   AND NOT tgisinternal \
                   AND tgenabled <> 'O'",
                &[table],
            )
            .await
            .map_err(|e| RunnerError::CatalogQueryFailed {
                query_label: "pg_trigger disabled scan",
                source: e,
            })?;
        if !disabled_rows.is_empty() {
            let mut triggers: Vec<(String, char)> = Vec::with_capacity(disabled_rows.len());
            for r in &disabled_rows {
                let name: String = r.try_get(0).unwrap_or_default();
                let raw: i8 = r.try_get(1).unwrap_or(0);
                // tgenabled is a single-byte CHAR (Postgres `char`)
                // mapped to Rust as `i8`; valid values are 'O', 'D',
                // 'R', 'A' — all positive ASCII. Re-interpret as
                // unsigned without bounds-checking the high half
                // because i8 cannot exceed 127.
                let ch = if raw >= 0 { (raw as u8) as char } else { '?' };
                triggers.push((name, ch));
            }
            return Err(RunnerError::PkFlipHazardDisabledTriggers {
                table: table.clone(),
                triggers,
            });
        }
    }

    // D063 — long-running transactions.
    let threshold = runner_ctx.config.pk_flip_long_tx_threshold_secs;
    if threshold > 0 {
        let long_rows = ctx
            .query_all(
                "SELECT pid, EXTRACT(EPOCH FROM (now() - xact_start))::bigint AS age \
                 FROM pg_stat_activity \
                 WHERE pid <> pg_backend_pid() \
                   AND xact_start IS NOT NULL \
                   AND now() - xact_start > make_interval(secs => $1::int)",
                &[&(threshold as i32)],
            )
            .await
            .map_err(|e| RunnerError::CatalogQueryFailed {
                query_label: "pg_stat_activity long tx scan",
                source: e,
            })?;
        if !long_rows.is_empty() {
            let mut offenders: Vec<(i32, i64)> = Vec::with_capacity(long_rows.len());
            for r in &long_rows {
                let pid: i32 = r.try_get(0).unwrap_or(0);
                let age: i64 = r.try_get(1).unwrap_or(0);
                offenders.push((pid, age));
            }
            return Err(RunnerError::PkFlipHazardLongRunningTx {
                offenders,
                threshold_secs: threshold,
            });
        }
    }

    Ok(())
}

/// Recover every table name appearing immediately after an
/// `ALTER TABLE` token in `sql`. Byte-level forward scan; no regex.
/// Identifier rule: ASCII letter or underscore as the first byte,
/// then ASCII alphanumerics or underscores, up to 63 bytes (the
/// Postgres `NAMEDATALEN - 1` ceiling). We accept double-quoted
/// identifiers too — the inner contents are passed through verbatim
/// (the descriptor / projection layer guarantees identifier shape
/// upstream so the contents survive interpretation as a plain
/// table name in `pg_class`).
fn scan_alter_table_targets(sql: &str) -> Vec<String> {
    const MARKER: &[u8] = b"ALTER TABLE ";
    let bytes = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i + MARKER.len() <= bytes.len() {
        if &bytes[i..i + MARKER.len()] == MARKER {
            let start = i + MARKER.len();
            // Identifier may be plain or double-quoted.
            let (id, end) = if start < bytes.len() && bytes[start] == b'"' {
                let id_start = start + 1;
                let mut j = id_start;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                if j > id_start && j < bytes.len() {
                    (
                        String::from_utf8_lossy(&bytes[id_start..j]).into_owned(),
                        j + 1,
                    )
                } else {
                    (String::new(), bytes.len())
                }
            } else {
                let id_start = start;
                let mut j = id_start;
                let mut len = 0usize;
                while j < bytes.len() && len < 63 {
                    let b = bytes[j];
                    let valid = if len == 0 {
                        b.is_ascii_alphabetic() || b == b'_'
                    } else {
                        b.is_ascii_alphanumeric() || b == b'_'
                    };
                    if !valid {
                        break;
                    }
                    j += 1;
                    len += 1;
                }
                if j > id_start {
                    (String::from_utf8_lossy(&bytes[id_start..j]).into_owned(), j)
                } else {
                    (String::new(), j)
                }
            };
            if !id.is_empty() {
                out.push(id);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn segment_kind_name(kind: SegmentKind) -> &'static str {
    match kind {
        SegmentKind::Transactional => "transactional",
        SegmentKind::NonTransactional => "non-transactional",
        SegmentKind::MetadataOnly => "metadata-only",
    }
}

#[allow(clippy::result_large_err)]
fn preflight_segment_sql_execution_compatibility(plan: &MigrationPlan) -> Result<(), RunnerError> {
    for (segment_index, segment) in plan.segments.iter().enumerate() {
        if segment.kind == SegmentKind::MetadataOnly {
            continue;
        }
        for statement in &segment.statements {
            if let Some(problem) =
                classify_segment_sql_execution_mode_problem(segment.kind, &statement.up)
            {
                return Err(RunnerError::SegmentSqlExecutionModeConflict {
                    segment_index,
                    segment_kind: segment.kind,
                    statement_label: statement.label.clone(),
                    problem,
                });
            }
        }
    }
    Ok(())
}

fn classify_segment_sql_execution_mode_problem(
    segment_kind: SegmentKind,
    sql: &str,
) -> Option<SegmentSqlExecutionModeProblem> {
    let bytes = sql.as_bytes();
    let mut idx = 0usize;
    let first = next_sql_leading_keyword(bytes, &mut idx)?;
    let second = next_sql_leading_keyword(bytes, &mut idx);
    let third = next_sql_leading_keyword(bytes, &mut idx);
    let fourth = next_sql_leading_keyword(bytes, &mut idx);

    if token_eq(first, "BEGIN") {
        return Some(SegmentSqlExecutionModeProblem::TransactionControl { keyword: "BEGIN" });
    }
    if token_eq(first, "START") && second.is_some_and(|tok| token_eq(tok, "TRANSACTION")) {
        return Some(SegmentSqlExecutionModeProblem::TransactionControl {
            keyword: "START TRANSACTION",
        });
    }
    if token_eq(first, "COMMIT") {
        return Some(SegmentSqlExecutionModeProblem::TransactionControl { keyword: "COMMIT" });
    }
    if token_eq(first, "ROLLBACK") {
        return Some(SegmentSqlExecutionModeProblem::TransactionControl {
            keyword: "ROLLBACK",
        });
    }
    if token_eq(first, "SAVEPOINT") {
        return Some(SegmentSqlExecutionModeProblem::TransactionControl {
            keyword: "SAVEPOINT",
        });
    }
    if token_eq(first, "RELEASE") && second.is_some_and(|tok| token_eq(tok, "SAVEPOINT")) {
        return Some(SegmentSqlExecutionModeProblem::TransactionControl {
            keyword: "RELEASE SAVEPOINT",
        });
    }

    if segment_kind != SegmentKind::Transactional {
        return None;
    }

    if token_eq(first, "CREATE")
        && second.is_some_and(|tok| token_eq(tok, "INDEX"))
        && third.is_some_and(|tok| token_eq(tok, "CONCURRENTLY"))
    {
        return Some(SegmentSqlExecutionModeProblem::RequiresNonTransactional {
            statement_shape: "CREATE INDEX CONCURRENTLY",
        });
    }
    if token_eq(first, "CREATE")
        && second.is_some_and(|tok| token_eq(tok, "UNIQUE"))
        && third.is_some_and(|tok| token_eq(tok, "INDEX"))
        && fourth.is_some_and(|tok| token_eq(tok, "CONCURRENTLY"))
    {
        return Some(SegmentSqlExecutionModeProblem::RequiresNonTransactional {
            statement_shape: "CREATE UNIQUE INDEX CONCURRENTLY",
        });
    }
    if token_eq(first, "DROP")
        && second.is_some_and(|tok| token_eq(tok, "INDEX"))
        && third.is_some_and(|tok| token_eq(tok, "CONCURRENTLY"))
    {
        return Some(SegmentSqlExecutionModeProblem::RequiresNonTransactional {
            statement_shape: "DROP INDEX CONCURRENTLY",
        });
    }

    None
}

fn next_sql_leading_keyword<'a>(bytes: &'a [u8], idx: &mut usize) -> Option<&'a [u8]> {
    skip_sql_leading_ws_and_comments(bytes, idx);
    if *idx >= bytes.len() || !is_sql_ident_start(bytes[*idx]) {
        return None;
    }
    let start = *idx;
    *idx += 1;
    while *idx < bytes.len() && is_sql_ident_continue(bytes[*idx]) {
        *idx += 1;
    }
    Some(&bytes[start..*idx])
}

fn skip_sql_leading_ws_and_comments(bytes: &[u8], idx: &mut usize) {
    loop {
        while *idx < bytes.len() && bytes[*idx].is_ascii_whitespace() {
            *idx += 1;
        }
        if *idx + 1 < bytes.len() && bytes[*idx] == b'-' && bytes[*idx + 1] == b'-' {
            *idx += 2;
            while *idx < bytes.len() && bytes[*idx] != b'\n' {
                *idx += 1;
            }
            continue;
        }
        if *idx + 1 < bytes.len() && bytes[*idx] == b'/' && bytes[*idx + 1] == b'*' {
            *idx += 2;
            let mut depth = 1usize;
            while *idx < bytes.len() && depth > 0 {
                if *idx + 1 < bytes.len() && bytes[*idx] == b'/' && bytes[*idx + 1] == b'*' {
                    depth += 1;
                    *idx += 2;
                    continue;
                }
                if *idx + 1 < bytes.len() && bytes[*idx] == b'*' && bytes[*idx + 1] == b'/' {
                    depth -= 1;
                    *idx += 2;
                    continue;
                }
                *idx += 1;
            }
            continue;
        }
        return;
    }
}

fn is_sql_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_sql_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn token_eq(token: &[u8], expected: &str) -> bool {
    token.eq_ignore_ascii_case(expected.as_bytes())
}

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
/// The label format is set by the SQL emitter — `AddIndex <name>`
/// for new indexes (see `sql.rs::emit_add_index`). The table name is
/// recovered from the SQL's `ON "<table>"` clause via byte-level
/// scanning — explicit forward scan over the bytes, no
/// pattern-matching engine.
fn parse_create_index_statement(stmt: &OperationSql) -> Option<(String, String)> {
    // Only AddIndex labels are eligible. The DropIndex labels start
    // with "DropIndex" so they cannot collide.
    let label = stmt.label.as_str();
    let index_name = label.strip_prefix("AddIndex ")?.to_string();

    // Extract the table name by scanning for the literal ` ON "`
    // marker followed by the quoted table name. Postgres `CREATE
    // INDEX` SQL always emits `ON "<table>"` — see emit_add_index.
    // Byte-level forward scan; no pattern-matching engine.
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

/// Build the initial `partial_apply_note` for a run, when the apply
/// is out-of-order and either the policy carries an override reason
/// or there's a known conflicting peer. Returns `None` for the
/// in-order common case. Pure function — no DB access (cluster-2
/// simplify Finding 7 — note-composition extraction).
fn compose_initial_note(
    is_out_of_order: bool,
    override_reason: Option<&str>,
    conflicting_peer: Option<&(String, Option<String>)>,
) -> Option<String> {
    match (is_out_of_order, override_reason) {
        (true, Some(reason)) if !reason.is_empty() => {
            let header = match conflicting_peer {
                Some((peer, Some(ts))) => {
                    format!("out-of-order apply (peer {peer} applied at {ts}) override: {reason}")
                }
                Some((peer, None)) => {
                    format!("out-of-order apply (peer {peer}) override: {reason}")
                }
                None => format!("out-of-order apply override: {reason}"),
            };
            Some(header)
        }
        (true, None) => match conflicting_peer {
            Some((peer, Some(ts))) => Some(format!(
                "out-of-order apply: peer {peer} was already applied at {ts}"
            )),
            Some((peer, None)) => Some(format!("out-of-order apply: peer {peer}")),
            None => None,
        },
        _ => None,
    }
}

/// Build the `partial_apply_note` string for a failed transactional
/// segment. Centralizes the inline format strings that previously
/// lived in `apply_plan_inner`'s match arm (cluster-2 simplify
/// Finding 7 — note-formatting extraction).
fn note_for_failed_transactional_segment(seg_idx: usize, e: &RunnerError) -> String {
    match e {
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
        RunnerError::PkFlipVerificationFailed {
            table,
            count_violating,
        } => format!(
            "PK-flip verification halt at segment {seg_idx}: table `{table}` \
             has {count_violating} row(s) with NULL or stale shadow values",
        ),
        other => format!("transactional segment {seg_idx} failed: {other}"),
    }
}

/// Bind both `heer.node_id` and `heer.ranj_node_id` on the pinned
/// migration connection using validation-backed SQL. This calls
/// HeeRanjID's `set_heer_node_id(<id>)` and `set_heer_ranj_node_id(<id>)`
/// functions, which validate that the node is registered and active
/// in `heer_nodes` before setting the session GUCs.
///
/// This must be called after Phase 0 installs HeeRanjID functions
/// (so `set_heer_node_id` exists) and before non-Phase-0
/// `generate_run_id` / `HeerId::generate`, user migration SQL, or
/// any ledger/progress/terminal/snapshot mutation that depends on
/// the current node identity.
pub(crate) async fn bind_runner_node_identity(
    ctx: &mut DjogiContext,
    node_id: i32,
) -> Result<(), RunnerError> {
    bind_runner_node_identity_steps(ctx, node_id)
        .await
        .map_err(|source| RunnerError::NodeIdentityBindingFailed { node_id, source })
}

async fn bind_runner_node_identity_steps(
    ctx: &mut DjogiContext,
    node_id: i32,
) -> Result<(), DjogiError> {
    // Bind heer.node_id first. Both GUCs bind to the same value unless
    // a future design explains why they should differ.
    // Uses parameterized execute ($1) for defense-in-depth: no string
    // interpolation of caller-supplied values into SQL. The $1 parameter
    // maps to i32 → PostgreSQL INTEGER via tokio-postgres type coercion.
    ctx.execute("SELECT set_heer_node_id($1)", &[&node_id])
        .await
        .map_err(|source| {
            crate::DjogiError::Db(crate::DbError::other(format!(
                "bind heer.node_id: {source}"
            )))
        })?;

    // Bind heer.ranj_node_id second. If this fails after the first GUC
    // succeeds, the pinned connection has partial GUC state (heer.node_id
    // set but heer.ranj_node_id not). However, the error prevents generate_run_id
    // and user SQL from executing, and the pinned connection is released with
    // transactional state cleaned up by the existing reconcile/release pattern.
    ctx.execute("SELECT set_heer_ranj_node_id($1)", &[&node_id])
        .await
        .map_err(|source| {
            crate::DjogiError::Db(crate::DbError::other(format!(
                "bind heer.ranj_node_id: {source}"
            )))
        })?;

    Ok(())
}

async fn provision_phase_zero_single_node_dev(
    ctx: &mut DjogiContext,
    node_id: i32,
) -> Result<(), RunnerError> {
    ctx.batch_execute(heeranjid::postgres_schema::SEED_SQL)
        .await
        .map_err(|source| RunnerError::SingleNodeDevProvisioningFailed {
            node_id,
            step: "seed node rows",
            source,
        })?;

    let node_seed_sql = super::bootstrap::compose_node_seed("", node_id).map_err(|source| {
        RunnerError::SingleNodeDevProvisioningFailed {
            node_id,
            step: "compose node GUC defaults",
            source: DjogiError::Db(DbError::other(source.to_string())),
        }
    })?;
    ctx.batch_execute(&node_seed_sql).await.map_err(|source| {
        RunnerError::SingleNodeDevProvisioningFailed {
            node_id,
            step: "set node GUC defaults",
            source,
        }
    })?;

    bind_runner_node_identity_steps(ctx, node_id)
        .await
        .map_err(|source| RunnerError::SingleNodeDevProvisioningFailed {
            node_id,
            step: "bind node identity",
            source,
        })
}

/// Generate a fresh `run_id` via the HeerId default-allocation path.
/// HeerId is a 64-bit time-ordered ID — perfect for the per-runner
/// invocation key, which we want to be unique, sortable, and stable
/// across machines.
/// **carve-out .** When `version` is the canonical
/// Bootstrap label (`super::bootstrap::PHASE_ZERO_VERSION`),
/// HeeRanjID is by definition not yet installed — is what
/// installs it. Calling `heerid_next()` would fail with "function
/// does not exist". For only, we fall back to a wall-clock
/// nanosecond-precision id derived from
/// `clock_timestamp() - epoch '2026-01-01'`, which fits in `i64`
/// for the next ~140 years and is unique-enough across the one-time
/// Emission per database. Subsequent migrations route
/// through the standard HeerId path because has by then
/// installed `heerid_next()`.
async fn generate_run_id(ctx: &mut DjogiContext, version: &str) -> Result<i64, RunnerError> {
    if version == super::bootstrap::PHASE_ZERO_VERSION {
        // Carve-out — HeerRanjID not yet installed at this
        // point. Fall back to a wall-clock nanosecond id. We use
        // `EXTRACT(EPOCH FROM clock_timestamp()) * 1e9` for nanosecond
        // resolution; the cast to BIGINT fits comfortably in i64
        // for the foreseeable future. Two concurrent applies
        // against the same workspace are impossible (the workspace
        // lock guarantees exclusion), so collision risk is zero.
        let row = ctx
            .query_one(
                "SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000000000)::BIGINT AS run_id",
                &[],
            )
            .await
            .map_err(|e| RunnerError::RunIdGenerationFailed { source: e })?;
        let id: i64 = row
            .try_get("run_id")
            .map_err(|e| RunnerError::RunIdGenerationFailed {
                source: crate::DjogiError::Db(crate::DbError::other(format!("decode run_id: {e}"))),
            })?;
        return Ok(id);
    }
    use crate::primary_key::PrimaryKeyDbGen;
    let id = HeerId::generate(ctx)
        .await
        .map_err(|e| RunnerError::RunIdGenerationFailed { source: e })?;
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
/// (unique_violation). Used by runner insert paths to classify
/// duplicate-version collisions into terminal vs. non-terminal
/// typed errors.
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

/// Classify a duplicate-version collision by loading the full ledger
/// row and dispatching by `status`.
/// Terminal statuses (`applied`, `faked`, `baseline`) surface the
/// existing `VersionAlreadyApplied` variant and preserve the
/// informational `applied_at` lookup. Non-terminal statuses
/// (`pending`, `failed`, `rolled_back`) surface
/// `VersionCollisionNonTerminal` with the blocking row identity.
/// If the readback lookup errors or returns `None` after a 23505,
/// surface a `LedgerQueryFailed` path explicitly rather than
/// pretending the row is already applied.
async fn classify_duplicate_version_collision(
    ctx: &mut DjogiContext,
    version: &str,
    app_label: &str,
) -> RunnerError {
    let row = match load_ledger_row_for_version(ctx, version, app_label).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return RunnerError::LedgerQueryFailed {
                query_label: "load_row_for_version",
                source: DjogiError::Db(DbError::other(format!(
                    "duplicate-version collision for `{version}` but \
                     no ledger row was returned by load_full_row_by_version",
                ))),
            };
        }
        Err(source) => {
            return RunnerError::LedgerQueryFailed {
                query_label: "load_row_for_version",
                source,
            };
        }
    };

    match row.status {
        LedgerStatus::Applied | LedgerStatus::Faked | LedgerStatus::Baseline => {
            let applied_at = load_applied_at(ctx, version, app_label).await;
            RunnerError::VersionAlreadyApplied {
                version: version.to_string(),
                applied_at,
            }
        }
        LedgerStatus::Pending | LedgerStatus::Failed | LedgerStatus::RolledBack => {
            RunnerError::VersionCollisionNonTerminal {
                version: row.version,
                status: row.status,
                run_id: row.run_id,
            }
        }
    }
}

/// Walk the bucket's existing applied / faked / baseline ledger
/// rows and surface the highest-version peer whose `version`
/// lexically exceeds `candidate_version`. Returns the peer's
/// `(version, applied_at_rfc3339)` tuple — or `None` when no peer
/// would conflict (the typical happy-path case).
/// **Why "applied / faked / baseline"?** These three statuses
/// represent rows that the runner has acknowledged as "this
/// migration is in the database" — `pending` and `failed` rows do
/// not, and rolled-back rows have explicitly opted out. Treating
/// rolled-back rows as conflicting peers would block re-applying a
/// reverted migration, which is a legitimate workflow.
/// **Lexical compare.** The version prefix is `V<14 ASCII digits>`
/// (timestamp-derived) so lexical order = chronological order. We
/// compare the full version string (including the `__<slug>` tail)
/// so two versions sharing the same timestamp prefix sort by their
/// slug, which keeps the comparison deterministic.
async fn find_higher_applied_version(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
    candidate_version: &str,
) -> Result<Option<(String, Option<String>)>, DjogiError> {
    // app_label is the bucket's per-app key — the per-database
    // dimension is implicit in which connection / pool the runner
    // routes through, mirroring the runner's single-pool stance.
    // When `DjogiContext::pool_for(database)` lands the per-database
    // routing comes for free; the SELECT still scopes by app_label.
    let row_opt = ctx
        .query_opt(
            "SELECT version, \
                    to_char(applied_at AT TIME ZONE 'UTC', \
                            'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS applied_at_rfc3339 \
             FROM djogi_schema_migrations \
             WHERE app_label = $1 \
               AND status IN ('applied', 'faked', 'baseline') \
               AND version > $2 \
             ORDER BY version DESC \
             LIMIT 1",
            &[&bucket.app, &candidate_version],
        )
        .await?;
    let Some(row) = row_opt else {
        return Ok(None);
    };
    let conflicting_version: String = row.try_get(0)?;
    let applied_at_rfc3339: Option<String> = row.try_get(1).ok();
    Ok(Some((conflicting_version, applied_at_rfc3339)))
}

async fn bucket_has_applied_history(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
) -> Result<bool, DjogiError> {
    let row = ctx
        .query_one(
            "SELECT EXISTS(SELECT 1 \
             FROM djogi_schema_migrations \
             WHERE app_label = $1 \
               AND status IN ('applied', 'faked', 'baseline'))",
            &[&bucket.app],
        )
        .await?;
    row.try_get(0).map_err(Into::into)
}

/// Read the `applied_at` timestamp of an existing row whose
/// `version` we just collided with. Returns `None` if the lookup
/// fails — the operator-facing message degrades gracefully because
/// the message is informational only.
async fn load_applied_at(
    ctx: &mut DjogiContext,
    version: &str,
    app_label: &str,
) -> Option<OffsetDateTime> {
    let row = ctx
        .query_opt(
            "SELECT applied_at FROM djogi_schema_migrations \
             WHERE version = $1 AND app_label = $2",
            &[&version, &app_label],
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

// ── partition leaf-placeholder expansion ─────────────────────────────

/// Controls how empty-leaf partitions are handled during expansion.
/// **ApplyLenient** (default for `apply_plan`): preserves the current
/// empty-leaf no-op fallback, emitting a comment-only statement so the
/// segment completes without error. The operator sees the diagnostic and
/// can attach partitions before retrying.
/// **ReplayStrict** (rollback / repair): refuses zero leaves because
/// replaying a shorter stream would silently miss leaf work that the
/// ledger already counted. Returns `RunnerError::PartitionExpansionNoLeaves`
/// so the caller can abort or handle the discrepancy explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartitionExpansionMode {
    ApplyLenient,
    ReplayStrict,
}

/// Crate-internal helper wrapping [`expand_partition_leaf_placeholders`].
/// Materializes partition leaf placeholders according to the given mode.
pub(crate) async fn materialize_execution_plan(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    mode: PartitionExpansionMode,
) -> Result<
    (
        MigrationPlan,
        std::collections::HashMap<String, Vec<String>>,
    ),
    RunnerError,
> {
    expand_partition_leaf_placeholders(ctx, plan, mode).await
}

/// Serialize a parent-to-leaves map into the compact `leaf_identity`
/// format stored in the ledger. Parents sorted alphabetically; leaves
/// already sorted by `regclass::text`. Newline-delimited
/// `parent:leaf1,leaf2` entries.
pub(crate) fn serialize_leaf_identity(
    leaves_cache: &std::collections::HashMap<String, Vec<String>>,
) -> Option<String> {
    let mut entries: Vec<_> = leaves_cache
        .iter()
        .map(|(parent, leaves)| format!("{}:{}", parent, leaves.join(",")))
        .collect();
    entries.sort();
    if entries.is_empty() {
        None
    } else {
        Some(entries.join("\n"))
    }
}

/// Compute the current parent-to-leaves cache for `plan` using LENIENT
/// partition expansion, suitable for a pre-strict leaf-identity check.
/// **Why lenient.** `apply_plan` records `leaf_identity` from a
/// `ApplyLenient` expansion (which tolerates zero leaves), so a fresh
/// lenient cache compares apples-to-apples against the stored value.
/// Crucially, lenient mode does NOT error on a zero-leaf parent — so a
/// zero-leaf↔non-empty topology drift surfaces as a *comparison*
/// mismatch (empty cache → empty serialized identity) rather than the
/// strict path's [`RunnerError::PartitionExpansionNoLeaves`]. Callers
/// run this before [`materialize_execution_plan`] with
/// [`PartitionExpansionMode::ReplayStrict`] so the leaf-identity
/// mismatch is reported as such instead of a strict zero-leaf refusal.
pub(crate) async fn compute_leaf_identity_cache(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
) -> Result<std::collections::HashMap<String, Vec<String>>, RunnerError> {
    let (_plan, cache) =
        expand_partition_leaf_placeholders(ctx, plan, PartitionExpansionMode::ApplyLenient).await?;
    Ok(cache)
}

/// Walk every segment and expand the `<EACH_LEAF_TABLE>` placeholder
/// inside any partitioned-flip statement into one concrete-leaf
/// statement per leaf, sorted by `regclass::text` for determinism.
/// Statements affected (by label prefix):
/// - `PkFlipPartitionedBackfill <parent>` — body carries
///   `CALL heeranjid_bulk_backfill('<EACH_LEAF_TABLE>', ...)`. Each
///   leaf gets its own CALL line.
/// - `PkFlipPartitionedIndex <parent>` — body carries the parent
///   UNIQUE-on-ONLY placeholder plus a comment block describing the
///   per-leaf `CREATE UNIQUE INDEX CONCURRENTLY` + `ALTER INDEX
/// ATTACH PARTITION` pattern. The comment is replaced with two
///   concrete statements per leaf.
///   Statements with no placeholder (`PkFlipPartitionedPrep`,
///   `PkFlipPartitionedCutover`) are passed through untouched.
///   **No regex.** Placeholder substitution uses byte-level
///   `String::replace` semantics with a fixed literal token. Per-leaf
///   statement composition uses straight string concatenation and the
///   `writeln!` macro.
///   **Failure modes.** If `pg_inherits` returns no leaves for a
///   declared partitioned parent, behavior depends on [`PartitionExpansionMode`]:
/// - ApplyLenient: produces a single comment line so the segment SQL
///   surfaces the empty-leaves state cleanly rather than running an
///   `<EACH_LEAF_TABLE>` literal that would fail with `undefined_table`.
///   Operator's job is to attach partitions before retrying.
/// - ReplayStrict: refuses zero leaves because rollback/repair must not
///   silently replay a shorter stream. Returns [`RunnerError::PartitionExpansionNoLeaves`].
async fn expand_partition_leaf_placeholders(
    ctx: &mut DjogiContext,
    plan: &MigrationPlan,
    mode: PartitionExpansionMode,
) -> Result<
    (
        MigrationPlan,
        std::collections::HashMap<String, Vec<String>>,
    ),
    RunnerError,
> {
    // Cache leaves per parent so multiple statements pointing at the
    // same partitioned parent share one query.
    let mut leaves_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    let mut new_plan = plan.clone();
    for segment in &mut new_plan.segments {
        let mut new_stmts: Vec<OperationSql> = Vec::with_capacity(segment.statements.len());
        for stmt in std::mem::take(&mut segment.statements) {
            let parent_for_label = partitioned_parent_from_label(&stmt.label);
            match parent_for_label {
                Some(parent) => {
                    if !leaves_cache.contains_key(&parent) {
                        let leaves = lookup_partition_leaves(ctx, &parent).await?;
                        leaves_cache.insert(parent.clone(), leaves);
                    }
                    let leaves = leaves_cache.get(&parent).expect("just inserted");
                    new_stmts.extend(expand_partition_statement(&stmt, &parent, leaves, mode)?);
                }
                None => new_stmts.push(stmt),
            }
        }
        segment.statements = new_stmts;
    }
    Ok((new_plan, leaves_cache))
}

/// Recover the partitioned parent name from a `PkFlipPartitioned…`
/// label. Returns `None` for non-partitioned labels (which carry no
/// `<EACH_LEAF_TABLE>` placeholder).
fn partitioned_parent_from_label(label: &str) -> Option<String> {
    // We expand only the labels whose bodies the emitter populates
    // with the `<EACH_LEAF_TABLE>` placeholder. Prep and cutover
    // operate on the partitioned parent directly via Postgres'
    // partition-aware DDL and do not need expansion.
    for prefix in [
        "PkFlipPartitionedBackfill ",
        "PkFlipPartitionedIndex ",
        "PkFlipPartitionedSelfFkIndex ",
    ] {
        if let Some(rest) = label.strip_prefix(prefix) {
            // Take the first whitespace-separated token — the parent
            // table name. Defensive: future labels may carry trailing
            // qualifiers (e.g. `(parent-level)` after expansion).
            let parent = rest.split_whitespace().next().unwrap_or(rest);
            return Some(parent.to_string());
        }
    }
    None
}

/// Query `pg_inherits` for the leaf partitions of `parent`,
/// deterministically sorted by `regclass::text`.
/// **Why a two-step lookup.** Postgres rejects a TEXT bind in the
/// binary-protocol position where it expects `regclass`
/// (tokio-postgres surfaces `WrongType { postgres: Regclass, rust:
/// "&str" }`). Resolving the parent's OID first via `to_regclass`
/// in a separate `query_one` lets us bind the OID as `oid` in the
/// second query against `pg_inherits`, matching the catalog's
/// column type exactly.
async fn lookup_partition_leaves(
    ctx: &mut DjogiContext,
    parent: &str,
) -> Result<Vec<String>, RunnerError> {
    // 1. Resolve parent's OID. `to_regclass` returns NULL for an
    // unknown / non-relation name; we surface that as an empty
    // leaf list so callers see "no leaves" rather than a hard
    // error during plan composition.
    let oid_row = ctx
        .query_one("SELECT to_regclass($1)::oid", &[&parent])
        .await
        .map_err(|e| RunnerError::CatalogQueryFailed {
            query_label: "to_regclass",
            source: e,
        })?;
    let oid_opt: Option<u32> = oid_row.try_get(0).ok();
    let Some(oid) = oid_opt else {
        return Ok(Vec::new());
    };

    // 2. Fetch leaves keyed off the resolved OID. `inhparent` is
    // `oid`; binding an `Oid` (u32 in tokio-postgres mapping)
    // matches by type and avoids the regclass binary-protocol
    // coercion path.
    let rows = ctx
        .query_all(
            "SELECT inhrelid::regclass::text \
             FROM pg_inherits \
             WHERE inhparent = $1 \
             ORDER BY inhrelid::regclass::text",
            &[&oid],
        )
        .await
        .map_err(|e| RunnerError::CatalogQueryFailed {
            query_label: "pg_inherits",
            source: e,
        })?;
    let mut out: Vec<String> = Vec::with_capacity(rows.len());
    for r in &rows {
        let leaf: String = r.try_get(0).unwrap_or_default();
        if !leaf.is_empty() {
            out.push(leaf);
        }
    }
    Ok(out)
}

/// Produce one concrete-leaf [`OperationSql`] per leaf for a
/// partitioned-flip statement. The original statement carries
/// `<EACH_LEAF_TABLE>` as a literal placeholder; we substitute and
/// emit one statement per leaf so the runner walks them in sequence.
/// For the index segment we ALSO emit the per-leaf `ATTACH PARTITION`
/// statement immediately after each leaf's CONCURRENTLY index build
/// so the partitioned parent's UNIQUE index becomes valid as soon as
/// the last leaf attaches.
// RunnerError is a large enum by design; this sync helper returns it by
// value and needs the suppression. Same rationale as handle_release_result.
#[allow(clippy::result_large_err)]
fn expand_partition_statement(
    stmt: &OperationSql,
    parent: &str,
    leaves: &[String],
    mode: PartitionExpansionMode,
) -> Result<Vec<OperationSql>, RunnerError> {
    if leaves.is_empty() {
        return match mode {
            PartitionExpansionMode::ApplyLenient => Ok(vec![OperationSql {
                label: format!("{} (no leaves)", stmt.label),
                up: format!(
                    "-- pg_inherits returned 0 leaves for partitioned parent {parent}; nothing to expand"
                ),
                down: stmt.down.clone(),
                lossy: stmt.lossy.clone(),
            }]),
            PartitionExpansionMode::ReplayStrict => Err(RunnerError::PartitionExpansionNoLeaves {
                parent: parent.to_string(),
                statement_label: stmt.label.clone(),
            }),
        };
    }

    let mut out: Vec<OperationSql> = Vec::with_capacity(leaves.len());

    if stmt.label.starts_with("PkFlipPartitionedBackfill ") {
        // The body carries one CALL with `<EACH_LEAF_TABLE>` plus a
        // multi-line comment header. Replace the placeholder once per
        // leaf and emit one OperationSql per CALL so the simple-query
        // batch never wraps multiple CALLs in a tx (the procedure's
        // internal COMMIT would fire `2D000`).
        for leaf in leaves {
            let body = stmt.up.replace("<EACH_LEAF_TABLE>", leaf);
            // Strip any trailing semicolon; runner_ctx dispatches this
            // through the internal single-statement batch path.
            let body = strip_trailing_semicolon(&body);
            // Prefer just the CALL line for the per-leaf statement so
            // the procedure runs cleanly without the multi-line
            // header comment muddying the per-statement record. We
            // recompose the comment as the first leaf's prefix only.
            let upper = format!(
                "-- partitioned backfill, leaf {leaf}\n{body}",
                leaf = leaf,
                body = extract_call_line(&body),
            );
            out.push(OperationSql {
                label: format!("PkFlipPartitionedBackfill {parent} leaf={leaf}"),
                up: upper,
                down: stmt.down.clone(),
                lossy: stmt.lossy.clone(),
            });
        }
        return Ok(out);
    }

    if stmt.label.starts_with("PkFlipPartitionedIndex ") {
        // The body carries the parent-level `CREATE UNIQUE INDEX … ON
        // ONLY <parent> (...)` plus a comment block describing the
        // per-leaf pattern. Keep the parent-level statement as the
        // FIRST OperationSql (transactional-friendly catalog op), then
        // emit one CONCURRENTLY + ATTACH per leaf.
        // Recover the parent index name from the parent-level
        // statement: it's `idx_<parent>_<part_col>_id_desc_idx` (or
        // similar). Rather than parse, pull the marker line that
        // begins with `CREATE UNIQUE INDEX ` and ends at the first `;`.
        let parent_stmt = extract_first_statement_starting_with(&stmt.up, "CREATE UNIQUE INDEX ");
        let parent_index_name = recover_parent_index_name(&parent_stmt);
        let (part_col, suffix) = recover_partition_columns(&parent_stmt);

        out.push(OperationSql {
            label: format!("PkFlipPartitionedIndex {parent} (parent-level)"),
            up: parent_stmt.clone(),
            down: stmt.down.clone(),
            lossy: stmt.lossy.clone(),
        });

        for leaf in leaves {
            // The CREATE INDEX target index name must be BARE (Postgres
            // forbids a schema-qualified index name; the index lands in
            // the leaf's own schema). DROP / ATTACH need the leaf's
            // schema re-attached.
            let (leaf_schema, leaf_bare) = split_leaf_schema(leaf);
            let leaf_idx = format!(
                "{leaf_bare}_{pkey}_id{suffix}_idx",
                pkey = part_col,
                suffix = suffix
            );
            let leaf_idx_qualified = match leaf_schema {
                Some(s) => format!("{s}.{leaf_idx}"),
                None => leaf_idx.clone(),
            };
            // The parent index lives in the parent's schema. The current
            // emitter always produces a bare parent table name, so this
            // returns `parent_index_name` unchanged in practice — but
            // using the parent's own schema is correct by design.
            let (parent_schema, _) = split_leaf_schema(parent);
            let parent_idx_qualified = match parent_schema {
                Some(s) => format!("{s}.{parent_index_name}"),
                None => parent_index_name.clone(),
            };
            let create_concurrent = format!(
                "CREATE UNIQUE INDEX CONCURRENTLY {leaf_idx} ON {leaf} ({pkey}, id{suffix})",
                leaf_idx = leaf_idx,
                leaf = leaf,
                pkey = part_col,
                suffix = suffix,
            );
            // Down SQL: drop the parent index first (CASCADE removes any
            // attached leaf partitions), then drop the leaf itself as a
            // no-op cleanup for the partial-apply case where the leaf was
            // created concurrently but never attached. Both statements use
            // IF EXISTS so they are safe to re-execute in subsequent reverse
            // steps after the parent was already dropped. Both index names
            // are schema-qualified for DROP.
            out.push(OperationSql {
                label: format!("PkFlipPartitionedIndex {parent} leaf={leaf} (concurrent)"),
                up: create_concurrent,
                down: format!(
                    "DROP INDEX IF EXISTS {parent_idx_qualified}; DROP INDEX IF EXISTS {leaf_idx_qualified}",
                ),
                lossy: None,
            });
            let attach = format!(
                "ALTER INDEX {parent_idx_qualified} ATTACH PARTITION {leaf_idx_qualified}",
            );
            out.push(OperationSql {
                label: format!("PkFlipPartitionedIndex {parent} leaf={leaf} (attach)"),
                up: attach,
                down: String::new(),
                lossy: None,
            });
        }
        return Ok(out);
    }

    if stmt.label.starts_with("PkFlipPartitionedSelfFkIndex ") {
        // Single-column self-FK index on a partitioned parent. Body
        // form: `CREATE INDEX <idx> ON ONLY <parent> (<col>_desc);`
        // plus a comment header. Mirror the PkFlipPartitionedIndex
        // expansion: keep the parent-level statement as the FIRST
        // OperationSql, then per-leaf CONCURRENTLY + ATTACH PARTITION.
        let parent_stmt = extract_first_statement_starting_with(&stmt.up, "CREATE INDEX ");
        let parent_index_name = recover_parent_index_name(&parent_stmt);
        let (col, suffix) = recover_self_fk_column(&parent_stmt);

        out.push(OperationSql {
            label: format!("PkFlipPartitionedSelfFkIndex {parent} (parent-level)"),
            up: parent_stmt.clone(),
            down: stmt.down.clone(),
            lossy: stmt.lossy.clone(),
        });

        for leaf in leaves {
            // CREATE target index name is bare; DROP / ATTACH re-attach
            // the leaf's schema. See PkFlipPartitionedIndex above.
            let (leaf_schema, leaf_bare) = split_leaf_schema(leaf);
            let leaf_idx = format!("{leaf_bare}_{col}{suffix}_idx", col = col, suffix = suffix);
            let leaf_idx_qualified = match leaf_schema {
                Some(s) => format!("{s}.{leaf_idx}"),
                None => leaf_idx.clone(),
            };
            let (parent_schema, _) = split_leaf_schema(parent);
            let parent_idx_qualified = match parent_schema {
                Some(s) => format!("{s}.{parent_index_name}"),
                None => parent_index_name.clone(),
            };
            let create_concurrent = format!(
                "CREATE INDEX CONCURRENTLY {leaf_idx} ON {leaf} ({col}{suffix})",
                leaf_idx = leaf_idx,
                leaf = leaf,
                col = col,
                suffix = suffix,
            );
            out.push(OperationSql {
                label: format!("PkFlipPartitionedSelfFkIndex {parent} leaf={leaf} (concurrent)"),
                up: create_concurrent,
                down: format!("DROP INDEX IF EXISTS {leaf_idx_qualified}"),
                lossy: None,
            });
            let attach = format!(
                "ALTER INDEX {parent_idx_qualified} ATTACH PARTITION {leaf_idx_qualified}",
            );
            out.push(OperationSql {
                label: format!("PkFlipPartitionedSelfFkIndex {parent} leaf={leaf} (attach)"),
                up: attach,
                down: String::new(),
                lossy: None,
            });
        }
        return Ok(out);
    }

    // Unknown partitioned label — pass through unchanged. Defensive:
    // future emitters that add new `PkFlipPartitioned…` labels keep
    // working without forcing this fn to learn about them.
    Ok(vec![stmt.clone()])
}

/// Strip a single trailing `;` (with optional trailing whitespace).
/// The runner's internal batch path accepts statements with or without a
/// terminator, but stripping keeps the per-leaf record tidy.
fn strip_trailing_semicolon(s: &str) -> String {
    let trimmed = s.trim_end();
    trimmed.strip_suffix(';').unwrap_or(trimmed).to_string()
}

/// Split a (possibly schema-qualified) relation name into its optional
/// schema prefix and bare relation name. `lookup_partition_leaves`
/// returns `regclass::text` values that Postgres schema-qualifies when
/// the leaf is not on `search_path` (`myschema.events_p_a`).
/// The separator is the LAST unquoted `.`; characters inside a
/// double-quoted identifier (`"weird.name"`) are skipped so an embedded
/// dot never splits a quoted segment. Returns `(Some(schema), bare)`
/// when an unquoted dot is present, else `(None, whole)`.
/// **Why the split matters.** `CREATE INDEX` forbids a schema-qualified
/// *index* name (the index lands in the table's schema), whereas
/// `DROP INDEX` and `ALTER INDEX … ATTACH PARTITION` accept and need the
/// qualification. Composing the leaf index name from `bare` and then
/// re-qualifying only for DROP/ATTACH keeps both forms correct.
/// **No regex.** Byte-level scan tracking quote state; no regex engine.
fn split_leaf_schema(qualified: &str) -> (Option<&str>, &str) {
    let bytes = qualified.as_bytes();
    let mut in_quote = false;
    let mut sep = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quote = !in_quote,
            b'.' if !in_quote => sep = Some(i),
            _ => {}
        }
    }
    match sep {
        Some(i) => (Some(&qualified[..i]), &qualified[i + 1..]),
        None => (None, qualified),
    }
}

/// Pull the FIRST line that contains `CALL heeranjid_bulk_backfill(`
/// from a multi-line body. Falls back to the whole body if the marker
/// is absent.
fn extract_call_line(s: &str) -> String {
    for line in s.lines() {
        if line.contains("CALL heeranjid_bulk_backfill(") {
            return strip_trailing_semicolon(line);
        }
    }
    strip_trailing_semicolon(s)
}

/// Pull the FIRST statement (terminated by the first `;`) that
/// starts with `start_marker`. Falls back to an empty string if not
/// found. Used to extract the parent-level `CREATE UNIQUE INDEX ON
/// ONLY <parent>` from the multi-line index segment body.
fn extract_first_statement_starting_with(body: &str, start_marker: &str) -> String {
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Skip leading whitespace + comments on this line.
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i + start_marker.len() <= bytes.len()
            && &bytes[i..i + start_marker.len()] == start_marker.as_bytes()
        {
            // Find the terminating `;`.
            let start = i;
            let mut j = i + start_marker.len();
            while j < bytes.len() && bytes[j] != b';' {
                j += 1;
            }
            // Include the `;` if present, normalize internal whitespace.
            let raw = String::from_utf8_lossy(&bytes[start..j]).into_owned();
            return raw;
        }
        // Skip to next line.
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        if i < bytes.len() {
            i += 1;
        }
    }
    String::new()
}

/// Recover the parent index name from a `CREATE [UNIQUE] INDEX
/// <name> ON ONLY ...` body. Tries the UNIQUE form first
/// (PkFlipPartitionedIndex composite PK indexes use it), then the
/// non-unique form (PkFlipPartitionedSelfFkIndex single-column
/// indexes use it). Falls back to an empty string if neither
/// matches.
fn recover_parent_index_name(parent_stmt: &str) -> String {
    let bytes = parent_stmt.as_bytes();
    for marker in [b"CREATE UNIQUE INDEX " as &[u8], b"CREATE INDEX " as &[u8]] {
        if bytes.len() < marker.len() {
            continue;
        }
        let mut i = 0usize;
        while i + marker.len() <= bytes.len() {
            if &bytes[i..i + marker.len()] == marker {
                let start = i + marker.len();
                let mut name_start = start;
                // Skip leading spaces.
                while name_start < bytes.len() && bytes[name_start] == b' ' {
                    name_start += 1;
                }
                // Skip optional CONCURRENTLY keyword (and any following
                // spaces). The parent-level emitter writes `ON ONLY` form
                // without CONCURRENTLY, but the per-leaf statements this
                // helper may also be handed do carry it — skip so the
                // captured name is the index, not the keyword.
                const CONC: &[u8] = b"CONCURRENTLY";
                if name_start + CONC.len() <= bytes.len()
                    && &bytes[name_start..name_start + CONC.len()] == CONC
                {
                    name_start += CONC.len();
                    while name_start < bytes.len() && bytes[name_start] == b' ' {
                        name_start += 1;
                    }
                }
                let mut j = name_start;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j > name_start {
                    return String::from_utf8_lossy(&bytes[name_start..j]).into_owned();
                }
                break;
            }
            i += 1;
        }
    }
    String::new()
}

/// Recover the partition column + shadow suffix from the parent
/// index statement. The expected form is
/// `CREATE UNIQUE INDEX <idx> ON ONLY <parent> (<pkey>, id<suffix>)`.
/// Returns `(pkey, suffix)`; falls back to `("partition_key", "_desc")`
/// when the body cannot be parsed.
fn recover_partition_columns(parent_stmt: &str) -> (String, String) {
    let bytes = parent_stmt.as_bytes();
    // Find `(` and `)`.
    let mut open = None;
    let mut close = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'(' && open.is_none() {
            open = Some(i + 1);
        } else if *b == b')' {
            close = Some(i);
            break;
        }
    }
    let (Some(o), Some(c)) = (open, close) else {
        return ("partition_key".to_string(), "_desc".to_string());
    };
    if o >= c {
        return ("partition_key".to_string(), "_desc".to_string());
    }
    let inside = String::from_utf8_lossy(&bytes[o..c]).into_owned();
    // Split by `,` and trim. First is pkey, second is `id<suffix>`.
    let mut parts = inside.splitn(2, ',');
    let pkey = parts.next().unwrap_or("").trim().to_string();
    let id_col = parts.next().unwrap_or("").trim();
    // `suffix` is the part after `id` — empty string is valid when the
    // column is plain `id` (no ordering suffix like `_desc`). Only fall
    // back when `id_col` does not start with `id` at all (parse failure).
    let suffix = match id_col.strip_prefix("id") {
        Some(s) => s.to_string(),
        None => return ("partition_key".to_string(), "_desc".to_string()),
    };
    if pkey.is_empty() {
        return ("partition_key".to_string(), "_desc".to_string());
    }
    (pkey, suffix)
}

/// Recover the self-FK column name + shadow suffix from the parent
/// self-FK index statement. The expected form is
/// `CREATE INDEX <idx> ON ONLY <parent> (<col>_desc)`. Returns
/// `(col, suffix)`; falls back to `("col", "_desc")` when the body
/// cannot be parsed.
fn recover_self_fk_column(parent_stmt: &str) -> (String, String) {
    let bytes = parent_stmt.as_bytes();
    let mut open = None;
    let mut close = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'(' && open.is_none() {
            open = Some(i + 1);
        } else if *b == b')' {
            close = Some(i);
            break;
        }
    }
    let (Some(o), Some(c)) = (open, close) else {
        return ("col".to_string(), "_desc".to_string());
    };
    if o >= c {
        return ("col".to_string(), "_desc".to_string());
    }
    let inside = parent_stmt[o..c].trim();
    // Single column form `<col>_desc`. Strip the `_desc` suffix.
    if let Some(col) = inside.strip_suffix("_desc")
        && !col.is_empty()
    {
        return (col.to_string(), "_desc".to_string());
    }
    ("col".to_string(), "_desc".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::config::MigrateConfig;
    use crate::migrate::bootstrap::PHASE_ZERO_VERSION;
    use crate::migrate::diff::Classification;
    use crate::migrate::projection::BucketKey;
    use crate::migrate::schema::AppliedSchema;
    use crate::migrate::segment::{MigrationPlan, Segment, SegmentKind};
    use crate::migrate::sql::OperationSql;
    use djogi_macros::djogi_test;

    fn bucket(db: &str, app: &str) -> BucketKey {
        BucketKey {
            database: db.to_string(),
            app: app.to_string(),
        }
    }

    fn empty_snapshot() -> AppliedSchema {
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-05-09T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        }
    }

    fn op(label: &str, up: &str) -> OperationSql {
        OperationSql {
            label: label.to_string(),
            up: up.to_string(),
            down: format!("-- down for {label}"),
            lossy: None,
        }
    }

    fn audit_plan() -> MigrationPlan {
        MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![
                Segment {
                    kind: SegmentKind::Transactional,
                    statements: vec![
                        op("AddTable audit_a", "CREATE TABLE audit_a (id bigint)"),
                        op("AddTable audit_b", "CREATE TABLE audit_b (id bigint)"),
                    ],
                },
                Segment {
                    kind: SegmentKind::MetadataOnly,
                    statements: vec![op("RenameApp ignored", "-- metadata-only placeholder")],
                },
                Segment {
                    kind: SegmentKind::NonTransactional,
                    statements: vec![op(
                        "AddIndex audit_a_id_idx",
                        "CREATE INDEX CONCURRENTLY audit_a_id_idx ON audit_a (id)",
                    )],
                },
            ],
        }
    }

    fn single_table_plan(table: &str) -> MigrationPlan {
        MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    &format!("AddTable {table}"),
                    &format!("CREATE TABLE {table} (id bigint)"),
                )],
            }],
        }
    }

    fn single_segment_plan(kind: SegmentKind, label: &str, up: &str) -> MigrationPlan {
        MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind,
                statements: vec![op(label, up)],
            }],
        }
    }

    fn runner_ctx_for_audit_with_snapshot_path(
        plan: &MigrationPlan,
        audit_pool: Option<deadpool_postgres::Pool>,
        snapshot_path: Option<PathBuf>,
    ) -> RunnerCtx {
        RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260509000000__audit_test".to_string(),
            description: "audit test".to_string(),
            checksum_up: compute_checksum_for_plan_up(plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool,
            runner_identity: Some(RunnerIdentity::SingleNodeDev), // test fixture — SingleNodeDev for non-P0 runner identity binding (G-gate requires it)
            drift_baseline: DriftBaseline::Disabled,
        }
    }

    /// Build a `RunnerCtx` for a drift-gate test: no snapshot persist
    /// (`snapshot: None`, `snapshot_path: None` — these tests assert gate
    /// behaviour, not snapshot writes), `SingleNodeDev` identity so the
    /// non-Phase-0 identity gate is satisfied, and the caller-chosen
    /// [`DriftBaseline`].
    fn runner_ctx_with_drift_baseline(
        plan: &MigrationPlan,
        version: &str,
        drift_baseline: DriftBaseline,
    ) -> RunnerCtx {
        RunnerCtx {
            bucket: plan.bucket.clone(),
            version: version.to_string(),
            description: "drift gate test".to_string(),
            checksum_up: compute_checksum_for_plan_up(plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::SingleNodeDev),
            drift_baseline,
        }
    }

    /// A `single_table_plan` with an explicitly-labelled bucket. The drift
    /// gate's history check keys on `app_label`, so a distinct app gives a
    /// distinct history bucket even on the shared test database.
    fn named_bucket_single_table_plan(db: &str, app: &str, table: &str) -> MigrationPlan {
        MigrationPlan {
            bucket: bucket(db, app),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(
                    &format!("AddTable {table}"),
                    &format!("CREATE TABLE {table} (id bigint)"),
                )],
            }],
        }
    }

    /// Build a one-table [`AppliedSchema`] baseline. The table is declared
    /// with a single `id BIGINT` PK column and nothing else. When the named
    /// table is absent from the live catalog, verifying this baseline yields a
    /// D601 (snapshot table missing in live, error-severity) diagnostic — so
    /// passing it to a never-applied bucket proves the gate self-skips rather
    /// than passing because the baseline happened to match.
    fn drift_baseline_with_table(table: &str) -> AppliedSchema {
        use crate::migrate::schema::{ColumnSchema, PkKindSchema, PrimaryKeySchema, TableSchema};

        let id_col = ColumnSchema {
            check: None,
            codec: None,
            comment: None,
            default_sql: None,
            foreign_key: None,
            generated: None,
            identity: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: "id".to_string(),
            nullable: false,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "BIGINT".to_string(),
            unique: false,
            type_change_using: None,
        };
        let table_schema = TableSchema {
            app: None,
            columns: vec![id_col],
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            storage_params: None,
            table: table.to_string(),
            table_comment: None,
            tablespace: None,
            tenant_key: None,
        };
        let mut snapshot = empty_snapshot();
        snapshot.models.insert(table.to_string(), table_schema);
        snapshot
    }

    fn unique_temp_path(tag: &str, ext: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("djogi-runner-{tag}-{stamp}.{ext}"))
    }

    fn acquire_test_workspace_guard() -> WorkspaceGuard {
        crate::migrate::acquire_workspace_lock(
            &unique_temp_path("audit", "lock"),
            Duration::from_secs(2),
        )
        .expect("acquire workspace lock")
    }

    async fn unreachable_pool_context() -> DjogiContext {
        let pool = crate::pg::pool::DjogiPool::builder("postgres://localhost:1/djogi_unreachable")
            .timeout(Duration::from_millis(1))
            .build()
            .await
            .expect("construct lazy unreachable pool");
        DjogiContext::from_pool(pool)
    }

    fn generated_stale_phase_zero_plan() -> MigrationPlan {
        let mut sql = crate::migrate::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            crate::migrate::bootstrap::DEFAULT_NODE_ID,
            true,
        )
        .expect("compose seed-capable Phase 0");
        let start = sql.find("DO $djogi$").expect("dynamic defaults block");
        let end = sql[start..]
            .find("SET heer.node_id = '1';")
            .map(|offset| start + offset)
            .expect("session SET after dynamic defaults");
        sql.replace_range(
            start..end,
            "ALTER DATABASE \"main\" SET heer.node_id = '1';\n\
             ALTER DATABASE \"main\" SET heer.ranj_node_id = '1';\n",
        );
        single_segment_plan(SegmentKind::Transactional, "Phase 0 bootstrap", &sql)
    }

    fn current_phase_zero_plan() -> MigrationPlan {
        let sql = crate::migrate::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            crate::migrate::bootstrap::DEFAULT_NODE_ID,
            false,
        )
        .expect("compose identity-free Phase 0");
        single_segment_plan(SegmentKind::Transactional, "Phase 0 bootstrap", &sql)
    }

    fn markerless_seed_phase_zero_plan() -> MigrationPlan {
        phase_zero_plan_with_seed_statement("INSERT INTO heer.heer_nodes (id) VALUES (1);")
    }

    fn phase_zero_plan_with_seed_statement(statement: &str) -> MigrationPlan {
        let mut sql = crate::migrate::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            crate::migrate::bootstrap::DEFAULT_NODE_ID,
            false,
        )
        .expect("compose identity-free Phase 0");
        sql.push('\n');
        sql.push_str(statement);
        sql.push('\n');
        single_segment_plan(SegmentKind::Transactional, "Phase 0 bootstrap", &sql)
    }

    fn extended_seed_statement_cases() -> [(&'static str, &'static str); 4] {
        [
            (
                "cte_insert",
                "WITH rows AS (SELECT 1) INSERT INTO heer.heer_nodes (id) VALUES (1);",
            ),
            (
                "cte_delete",
                "WITH moved AS (DELETE FROM heer.heer_node_state RETURNING *) SELECT 1;",
            ),
            (
                "merge",
                "MERGE INTO heer.heer_nodes AS target USING incoming ON false WHEN NOT MATCHED THEN INSERT (id) VALUES (1);",
            ),
            (
                "copy_from",
                "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM STDIN;",
            ),
        ]
    }

    fn seed_capable_phase_zero_plan() -> MigrationPlan {
        let sql = crate::migrate::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            crate::migrate::bootstrap::DEFAULT_NODE_ID,
            true,
        )
        .expect("compose seed-capable Phase 0");
        single_segment_plan(SegmentKind::Transactional, "Phase 0 bootstrap", &sql)
    }

    struct SigningKeyEnvUnsetGuard {
        previous: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    struct SigningKeyEnvReadGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl SigningKeyEnvReadGuard {
        fn hold() -> Self {
            Self {
                _guard: crate::snapshot::sign::SIGNING_KEY_ENV_MUTEX
                    .lock()
                    .expect("signing-key env mutex"),
            }
        }
    }

    impl SigningKeyEnvUnsetGuard {
        fn unset() -> Self {
            let guard = crate::snapshot::sign::SIGNING_KEY_ENV_MUTEX
                .lock()
                .expect("signing-key env mutex");
            let previous = std::env::var_os("DJOGI_SNAPSHOT_SIGNING_KEY");
            // SAFETY: `SIGNING_KEY_ENV_MUTEX` serialises every unit test in
            // this crate that reads or mutates `DJOGI_SNAPSHOT_SIGNING_KEY`.
            unsafe {
                std::env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
            }
            Self {
                previous,
                _guard: guard,
            }
        }
    }

    impl Drop for SigningKeyEnvUnsetGuard {
        fn drop(&mut self) {
            // SAFETY: this guard still holds `SIGNING_KEY_ENV_MUTEX`, so no
            // sibling unit test can concurrently read or mutate the signing
            // key env var while restoration happens.
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var("DJOGI_SNAPSHOT_SIGNING_KEY", previous);
                } else {
                    std::env::remove_var("DJOGI_SNAPSHOT_SIGNING_KEY");
                }
            }
        }
    }

    fn db_err(message: &'static str) -> DjogiError {
        DjogiError::Db(DbError::other(message))
    }

    /// `RunnerError::is_operator_actionable()` must classify every current
    /// variant consistently: checksum/domain/PK-flip/preflight refusals are
    /// operator-actionable (`true`), while transient I/O / backend failures
    /// remain retryable (`false`).
    #[test]
    fn runner_error_operator_actionable_variants() {
        let cases = [
            (
                RunnerError::ChecksumMismatch(djogi::migrate::ledger::ChecksumMismatch {
                    version: "V20260101000000__add_users".to_string(),
                    expected:
                        "V1:11111111111111111111111111111111111111111111111111111111111111111111"
                            .to_string(),
                    actual:
                        "V1:22222222222222222222222222222222222222222222222222222222222222222222"
                            .to_string(),
                }),
                true,
            ),
            (
                RunnerError::ChecksumFormat(djogi::migrate::ledger::ChecksumFormatError {
                    side: djogi::migrate::ledger::ChecksumSide::Expected,
                    value: "V2:deadbeef".to_string(),
                    kind: djogi::migrate::ledger::ChecksumFormatErrorKind::WrongPrefix,
                }),
                true,
            ),
            (
                RunnerError::VersionAlreadyApplied {
                    version: "V20260101000000__add_users".to_string(),
                    applied_at: None,
                },
                true,
            ),
            (
                RunnerError::VersionCollisionNonTerminal {
                    version: "V20260101000000__add_users".to_string(),
                    status: LedgerStatus::Pending,
                    run_id: 1,
                },
                true,
            ),
            (
                RunnerError::TargetTableNotFound {
                    bucket: bucket("main", ""),
                    index_name: "users_email_idx".to_string(),
                    target_table: "users".to_string(),
                },
                true,
            ),
            (
                RunnerError::RelpagesThresholdExceeded {
                    bucket: bucket("main", ""),
                    index_name: "users_email_idx".to_string(),
                    target_table: "users".to_string(),
                    relpages: 4096,
                    threshold: 128,
                },
                true,
            ),
            (
                RunnerError::SegmentSqlExecutionModeConflict {
                    segment_index: 0,
                    segment_kind: SegmentKind::Transactional,
                    statement_label: "CREATE TRIGGER users_idx".to_string(),
                    problem: SegmentSqlExecutionModeProblem::RequiresNonTransactional {
                        statement_shape: "requires non-transactional execution",
                    },
                },
                true,
            ),
            (
                RunnerError::SnapshotPersistFailed {
                    path: "schema_snapshot.json".into(),
                    source: SnapshotError::Io {
                        path: None,
                        source: std::io::Error::other("disk full"),
                    },
                },
                true,
            ),
            (RunnerError::BaselineSnapshotShouldNotBeProvided, true),
            (
                RunnerError::StalePhaseZeroArtifact {
                    version: "V00000000000000__phase_zero".to_string(),
                    refusal_reason: "generated-stale",
                },
                true,
            ),
            (
                RunnerError::DriftDetected {
                    bucket: bucket("main", "billing"),
                    report: crate::migrate::VerifyReport {
                        diagnostics: vec![crate::migrate::VerifyDiagnostic {
                            code: "D601".to_string(),
                            severity: crate::migrate::VerifySeverity::Error,
                            message: "Snapshot table missing from live DB".to_string(),
                            location: Some("billing.invoices".to_string()),
                        }],
                        latest_applied_version: Some("V20260601000000__billing".to_string()),
                        applied_count: 2,
                        unfinished_count: 0,
                    },
                },
                true,
            ),
            (
                RunnerError::DriftBaselineMissing {
                    bucket: bucket("main", "billing"),
                },
                true,
            ),
            (
                RunnerError::DriftBaselineCorrupted {
                    bucket: bucket("main", "billing"),
                    reason: String::new(),
                },
                true,
            ),
            (
                RunnerError::OutOfOrderRejected {
                    version: "V20260101000000__add_users".to_string(),
                    conflicting_version: "V20260102000000__add_orders".to_string(),
                    conflicting_applied_at: None,
                },
                true,
            ),
            (
                RunnerError::PkFlipHazardReplicaSessions {
                    walsenders: vec![("wal_sender".to_string(), "127.0.0.1:5432".to_string())],
                    subscriptions: vec!["replication_slot".to_string()],
                },
                true,
            ),
            (
                RunnerError::PkFlipHazardPreexistingZzzTrigger {
                    table: "public.events".to_string(),
                    trigger_names: vec!["zzz_rv_users_id".to_string()],
                },
                true,
            ),
            (
                RunnerError::PkFlipHazardDisabledTriggers {
                    table: "public.events".to_string(),
                    triggers: vec![("zzz_rv_events_id".to_string(), 'D')],
                },
                true,
            ),
            (
                RunnerError::PkFlipHazardLongRunningTx {
                    offenders: vec![(1234, 120)],
                    threshold_secs: 60,
                },
                true,
            ),
            (
                RunnerError::PkFlipVerificationFailed {
                    table: "public.events".to_string(),
                    count_violating: 2,
                },
                true,
            ),
            (
                RunnerError::AdvisoryUnlockReturnedFalse {
                    key: 0x0102_0304_0506_0708,
                    bucket: bucket("main", ""),
                },
                true,
            ),
            (
                RunnerError::PartitionExpansionNoLeaves {
                    parent: "events".to_string(),
                    statement_label: "migrate_partitions".to_string(),
                },
                true,
            ),
            (
                RunnerError::MissingApplyIdentity {
                    version: "V20260101000000__add_users".to_string(),
                },
                true,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.is_operator_actionable(), expected, "{err}");
        }
    }

    #[test]
    fn runner_error_non_operator_actionable_variants() {
        let cases = [
            (
                RunnerError::LockTimeout {
                    path: PathBuf::from("/tmp/.djogi-migrations-lock"),
                    holder_pid: Some(99),
                },
                false,
            ),
            (
                RunnerError::GuardError(crate::migrate::guard::GuardError::Io {
                    path: PathBuf::from("/tmp/.djogi-migrations-lock"),
                    source: std::io::Error::other("permission denied"),
                }),
                false,
            ),
            (
                RunnerError::AdvisoryLockFailed {
                    bucket: bucket("main", ""),
                    key: 0x0000_0001_0000_0001,
                    attempts: 3,
                },
                false,
            ),
            (
                RunnerError::AdvisoryLockQueryFailed {
                    app_label: "main".to_string(),
                    source: db_err("advisory lock query failed"),
                },
                false,
            ),
            (
                RunnerError::LedgerWriteFailed {
                    version: "V20260101000000__add_users".to_string(),
                    source: db_err("insert failed"),
                },
                false,
            ),
            (
                RunnerError::LedgerQueryFailed {
                    query_label: "load_row_for_version",
                    source: db_err("query failed"),
                },
                false,
            ),
            (
                RunnerError::RunIdGenerationFailed {
                    source: db_err("sequence exhausted"),
                },
                false,
            ),
            (
                RunnerError::NodeIdentityBindingFailed {
                    node_id: 1,
                    source: db_err("identity bind failed"),
                },
                false,
            ),
            (
                RunnerError::SingleNodeDevProvisioningFailed {
                    node_id: 1,
                    step: "register node",
                    source: db_err("provisioning failed"),
                },
                false,
            ),
            (
                RunnerError::LedgerBootstrapFailed {
                    source: db_err("create table failed"),
                },
                false,
            ),
            (
                RunnerError::TransactionalSegmentFailed {
                    segment_index: 0,
                    statement_label: "AddTable users".to_string(),
                    source: db_err("transaction failed"),
                },
                false,
            ),
            (
                RunnerError::NonTransactionalSegmentFailed {
                    segment_index: 0,
                    step_index: 0,
                    statement_label: "INSERT user".to_string(),
                    applied_steps_count: 0,
                    source: db_err("statement failed"),
                },
                false,
            ),
            (
                RunnerError::NonTransactionalProgressAckFailed {
                    segment_index: 0,
                    step_index: 0,
                    statement_label: "INSERT user".to_string(),
                    applied_steps_count: 0,
                    source: db_err("ack failed"),
                },
                false,
            ),
            (
                RunnerError::ConfigLoadFailed {
                    source: figment::Error::from("invalid config"),
                },
                false,
            ),
            (
                RunnerError::BaselineProjectionFailed {
                    source: Box::new(djogi::migrate::verify::VerifyRunError::CatalogQueryFailed {
                        query_label: "pg_namespace",
                        source: db_err("verify catalog query failed"),
                    }),
                },
                false,
            ),
            (
                RunnerError::DriftPreflightFailed {
                    source: Box::new(djogi::migrate::verify::VerifyRunError::CatalogQueryFailed {
                        query_label: "columns",
                        source: db_err("drift preflight failed"),
                    }),
                },
                false,
            ),
            (
                RunnerError::CatalogQueryFailed {
                    query_label: "pg_class",
                    source: db_err("catalog query failed"),
                },
                false,
            ),
            (
                RunnerError::PinnedSessionCheckoutFailed {
                    source: db_err("pool checkout failed"),
                },
                false,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.is_operator_actionable(), expected, "{err}");
        }
    }

    // ── drift RunnerError Display / source ───────────────────────────────
    // These pin the operator-facing text and `source()` chaining for the
    // three apply-time drift refusals so a future Display edit cannot
    // silently drop the bucket identity, the error-severity count, or the
    // next-step commands operators rely on.

    #[test]
    fn drift_detected_display_names_bucket_and_error_count_and_next_step() {
        let e = RunnerError::DriftDetected {
            bucket: bucket("main", "billing"),
            report: crate::migrate::verify::VerifyReport {
                diagnostics: vec![crate::migrate::verify::VerifyDiagnostic {
                    code: "D601".to_string(),
                    severity: crate::migrate::verify::VerifySeverity::Error,
                    message: "snapshot table `users` missing in live".to_string(),
                    location: Some("users".to_string()),
                }],
                latest_applied_version: Some("V20260101000000__x".to_string()),
                applied_count: 3,
                unfinished_count: 0,
            },
        };
        let msg = e.to_string();
        assert!(msg.contains("database=main"), "got: {msg}");
        assert!(msg.contains("app=billing"), "got: {msg}");
        assert!(msg.contains("1 error-severity"), "got: {msg}");
        assert!(msg.contains("djogi migrations verify"), "got: {msg}");
        assert!(msg.contains("djogi migrations attune"), "got: {msg}");
        assert!(std::error::Error::source(&e).is_none());
    }

    #[test]
    fn drift_preflight_failed_exposes_verify_error_as_source() {
        let e = RunnerError::DriftPreflightFailed {
            source: Box::new(crate::migrate::verify::VerifyRunError::LedgerQueryFailed {
                source: db_err("ledger read failed in test"),
            }),
        };
        assert!(e.to_string().contains("drift pre-flight could not run"));
        assert!(std::error::Error::source(&e).is_some());
    }

    #[test]
    fn drift_baseline_missing_display_names_bucket_and_recovery_commands() {
        let e = RunnerError::DriftBaselineMissing {
            bucket: bucket("main", "billing"),
        };
        let msg = e.to_string();
        assert!(msg.contains("database=main"), "got: {msg}");
        assert!(msg.contains("app=billing"), "got: {msg}");
        assert!(msg.contains("applied migration history"), "got: {msg}");
        assert!(msg.contains("schema_snapshot.json"), "got: {msg}");
        assert!(
            msg.contains("djogi migrations repair snapshot-rebuild"),
            "got: {msg}"
        );
        assert!(std::error::Error::source(&e).is_none());
    }

    #[test]
    fn drift_baseline_corrupted_display_names_bucket_and_recovery_commands() {
        let e = RunnerError::DriftBaselineCorrupted {
            bucket: bucket("main", "billing"),
            reason: "unexpected end of input".to_string(),
        };
        let msg = e.to_string();
        assert!(msg.contains("database=main"), "got: {msg}");
        assert!(msg.contains("app=billing"), "got: {msg}");
        assert!(msg.contains("schema_snapshot.json"), "got: {msg}");
        assert!(msg.contains("repair snapshot-rebuild"), "got: {msg}");
        assert!(std::error::Error::source(&e).is_none());
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
        // ("ab", "c") -> "abc"
        // ("a", "bc") -> "abc"
        // The `\0` separator means the two cannot collide.
        let a = advisory_lock_key(&bucket("ab", "c"));
        let b = advisory_lock_key(&bucket("a", "bc"));
        assert_ne!(a, b);
    }

    #[test]
    fn advisory_lock_key_pins_big_endian_byte_decode() {
        // Spec-pinned reference value. The test exists to lock in the
        // big-endian decode of the first 8 SHA-256 digest bytes per
        // the v3 contract — an alternate-language
        // implementation following the same spec must compute the
        // same i64 for these inputs.
        // Reference computation (also verified offline with Python):
        // d = sha256(b"djogi:advisory_lock:" + b"test_db" + b"\x00" + b"test_app")
        // bytes[..8] = 7d 01 ce 8f 30 91 ba 37
        // big-endian i64 = 0x7d01ce8f3091ba37 = 9_007_707_844_108_204_599
        // little-endian (the WRONG decode) would produce
        // 4_015_681_655_511_318_909.
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

    #[test]
    fn heer_id_non_zero_round_trips_directly() {
        // the real conversion logic — every implementation maps zero to
        // zero. Pin a non-trivial bit pattern through both paths to
        // catch a future Display-vs-as_i64 drift that the zero case
        // would silently pass. HeerId only exposes a fallible
        // `TryFrom<i64>` (positive 64-bit values only); pick a
        // representative positive bit pattern.
        let v: i64 = 0x0123_4567_89AB_CDEF_i64;
        let id = HeerId::try_from(v).expect("positive i64 round-trips through HeerId");
        assert_eq!(id.as_i64(), v);
        let via_from: i64 = i64::from(id);
        assert_eq!(via_from, v);
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

    // ── segment SQL execution-mode preflight (#286) ─────────────────────

    #[test]
    fn segment_sql_preflight_rejects_concurrent_index_in_transactional_segment() {
        let plan = single_segment_plan(
            SegmentKind::Transactional,
            "AddIndex users_email_idx",
            "CREATE INDEX CONCURRENTLY users_email_idx ON users (email);",
        );

        let err = preflight_segment_sql_execution_compatibility(&plan).expect_err("must reject");
        match err {
            RunnerError::SegmentSqlExecutionModeConflict {
                segment_index,
                segment_kind,
                statement_label,
                problem,
            } => {
                assert_eq!(segment_index, 0);
                assert_eq!(segment_kind, SegmentKind::Transactional);
                assert_eq!(statement_label, "AddIndex users_email_idx");
                assert_eq!(
                    problem,
                    SegmentSqlExecutionModeProblem::RequiresNonTransactional {
                        statement_shape: "CREATE INDEX CONCURRENTLY",
                    }
                );
            }
            other => panic!("expected SegmentSqlExecutionModeConflict, got {other:?}"),
        }
    }

    #[test]
    fn segment_sql_preflight_rejects_comment_separated_concurrent_index_keywords() {
        let plan = single_segment_plan(
            SegmentKind::Transactional,
            "AddIndex users_email_idx",
            "CREATE /* split */ UNIQUE INDEX /* still split */ CONCURRENTLY \
             users_email_idx ON users (email);",
        );

        let err = preflight_segment_sql_execution_compatibility(&plan).expect_err("must reject");
        match err {
            RunnerError::SegmentSqlExecutionModeConflict {
                segment_index,
                segment_kind,
                statement_label,
                problem,
            } => {
                assert_eq!(segment_index, 0);
                assert_eq!(segment_kind, SegmentKind::Transactional);
                assert_eq!(statement_label, "AddIndex users_email_idx");
                assert_eq!(
                    problem,
                    SegmentSqlExecutionModeProblem::RequiresNonTransactional {
                        statement_shape: "CREATE UNIQUE INDEX CONCURRENTLY",
                    }
                );
            }
            other => panic!("expected SegmentSqlExecutionModeConflict, got {other:?}"),
        }
    }

    #[test]
    fn segment_sql_preflight_rejects_begin_in_transactional_segment() {
        let plan = single_segment_plan(SegmentKind::Transactional, "manual begin", "BEGIN;");

        let err = preflight_segment_sql_execution_compatibility(&plan).expect_err("must reject");
        match err {
            RunnerError::SegmentSqlExecutionModeConflict {
                segment_index,
                segment_kind,
                statement_label,
                problem,
            } => {
                assert_eq!(segment_index, 0);
                assert_eq!(segment_kind, SegmentKind::Transactional);
                assert_eq!(statement_label, "manual begin");
                assert_eq!(
                    problem,
                    SegmentSqlExecutionModeProblem::TransactionControl { keyword: "BEGIN" }
                );
            }
            other => panic!("expected SegmentSqlExecutionModeConflict, got {other:?}"),
        }
    }

    #[test]
    fn segment_sql_preflight_rejects_savepoint_in_non_transactional_segment() {
        let plan = single_segment_plan(
            SegmentKind::NonTransactional,
            "manual savepoint",
            "SAVEPOINT retry_guard;",
        );

        let err = preflight_segment_sql_execution_compatibility(&plan).expect_err("must reject");
        match err {
            RunnerError::SegmentSqlExecutionModeConflict {
                segment_index,
                segment_kind,
                statement_label,
                problem,
            } => {
                assert_eq!(segment_index, 0);
                assert_eq!(segment_kind, SegmentKind::NonTransactional);
                assert_eq!(statement_label, "manual savepoint");
                assert_eq!(
                    problem,
                    SegmentSqlExecutionModeProblem::TransactionControl {
                        keyword: "SAVEPOINT",
                    }
                );
            }
            other => panic!("expected SegmentSqlExecutionModeConflict, got {other:?}"),
        }
    }

    #[test]
    fn segment_sql_preflight_allows_set_constraints_in_transactional_segment() {
        let plan = single_segment_plan(
            SegmentKind::Transactional,
            "defer constraints",
            "SET CONSTRAINTS ALL DEFERRED;",
        );

        preflight_segment_sql_execution_compatibility(&plan).expect("set constraints allowed");
    }

    #[test]
    fn segment_sql_preflight_ignores_function_body_begin_commit_tokens() {
        let plan = single_segment_plan(
            SegmentKind::Transactional,
            "install function",
            "-- leading comment mentioning BEGIN\n\
             CREATE OR REPLACE FUNCTION public.bump_counter()\n\
             RETURNS trigger AS $body$\n\
             BEGIN\n\
                 NEW.counter := COALESCE(NEW.counter, 0) + 1;\n\
                 RETURN NEW;\n\
             END;\n\
             $body$ LANGUAGE plpgsql;",
        );

        preflight_segment_sql_execution_compatibility(&plan)
            .expect("function body BEGIN/END must not trip preflight");
    }

    // ── is_unique_violation classifier ───────────────────────────────────

    #[test]
    fn is_unique_violation_rejects_non_db_errors() {
        // Anything that is not a DjogiError::Db must classify as
        // false — only a Db error carries a SQLSTATE.
        let nf = DjogiError::not_found("users");
        assert!(!is_unique_violation(&nf));
    }

    // ── checksum_for_baseline_snapshot ────────────────────────────

    #[test]
    fn checksum_for_baseline_snapshot_is_deterministic() {
        use crate::migrate::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
        use std::collections::BTreeMap;
        let snap = AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        };
        let a = checksum_for_baseline_snapshot(&snap);
        let b = checksum_for_baseline_snapshot(&snap);
        assert_eq!(a, b, "same input must produce same checksum");
        assert!(a.starts_with(super::ledger::CHECKSUM_PREFIX));
        assert_eq!(a.len(), super::ledger::CHECKSUM_LEN);
    }

    #[test]
    fn checksum_for_baseline_snapshot_changes_on_schema_change() {
        use crate::migrate::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
        use std::collections::BTreeMap;
        let mut a = AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        };
        let cs_a = checksum_for_baseline_snapshot(&a);
        a.registered_apps.push("billing".to_string());
        let cs_b = checksum_for_baseline_snapshot(&a);
        assert_ne!(cs_a, cs_b, "schema change must yield different checksum");
    }

    // ── BaselineSnapshotShouldNotBeProvided guard ─────────────────

    #[test]
    fn baseline_snapshot_should_not_be_provided_renders_message() {
        let e = RunnerError::BaselineSnapshotShouldNotBeProvided;
        let msg = format!("{e}");
        assert!(msg.contains("baseline_plan rejects caller-supplied snapshots"));
        assert!(msg.contains("snapshot = None"));
    }

    #[test]
    fn version_collision_non_terminal_renders_status_and_run_id() {
        for (status, guidance) in [
            (
                LedgerStatus::Pending,
                "then `repair_partial_apply` to resolve it in place",
            ),
            (
                LedgerStatus::Failed,
                "then `repair_resume_partial_apply` if it is still resumable or `repair_partial_apply` otherwise",
            ),
            (
                LedgerStatus::RolledBack,
                "the rolled_back row is retained as audit history",
            ),
        ] {
            let e = RunnerError::VersionCollisionNonTerminal {
                version: "V20260524010101__example".to_string(),
                status,
                run_id: 4242,
            };
            let msg = format!("{e}");
            assert!(msg.contains("V20260524010101__example"));
            assert!(msg.contains(status.as_db_str()));
            assert!(msg.contains("run_id 4242"));
            assert!(msg.contains("djogi migrations status"));
            assert!(msg.contains(guidance));
        }
    }

    #[test]
    fn version_collision_non_terminal_has_no_error_source() {
        let e = RunnerError::VersionCollisionNonTerminal {
            version: "V20260524010101__example".to_string(),
            status: LedgerStatus::Pending,
            run_id: 7,
        };
        assert!(std::error::Error::source(&e).is_none());
    }

    // ── apply_plan DDL audit wiring ────────────────────────────────

    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_plan_writes_audit_rows_for_executed_segments_when_key_unset(
        mut ctx: DjogiContext,
    ) {
        let _signing_key_env = SigningKeyEnvUnsetGuard::unset();
        let plan = audit_plan();
        let audit_pool = ctx
            .share_pool()
            .expect("djogi_test context should be pool-backed")
            .inner;
        let snapshot_path = unique_temp_path("audit-happy-path", "json");
        let cleanup_path = snapshot_path.clone();
        let runner_ctx =
            runner_ctx_for_audit_with_snapshot_path(&plan, Some(audit_pool), Some(snapshot_path));
        let guard = acquire_test_workspace_guard();
        let expected_sig = audit_signature_hex_for_snapshot(
            runner_ctx.snapshot.as_ref().expect("test snapshot"),
            [0u8; 32],
        )
        .expect("expected audit signature");

        let report = apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect("apply should write audit rows");
        assert_eq!(report.transactional_segments, 1);
        assert_eq!(report.non_transactional_segments, 1);
        assert_eq!(report.metadata_segments, 1);

        let rows = ctx
            .query_all(
                "SELECT target_database, app_label, ddl_sql, snapshot_signature_hex, \
                        applied_at <= lead(applied_at) OVER (ORDER BY id) AS applied_before_next \
                 FROM djogi_ddl_audit ORDER BY id",
                &[],
            )
            .await
            .expect("read audit rows");
        assert_eq!(
            rows.len(),
            2,
            "transactional and non-transactional segments should be audited; metadata-only skipped"
        );

        let first_sql: String = rows[0].try_get("ddl_sql").expect("first ddl_sql");
        assert!(
            first_sql.contains("CREATE TABLE audit_a")
                && first_sql.contains(";\n")
                && first_sql.contains("CREATE TABLE audit_b"),
            "transactional segment should store concatenated statement SQL; got {first_sql}"
        );
        let second_sql: String = rows[1].try_get("ddl_sql").expect("second ddl_sql");
        assert!(
            second_sql.contains("CREATE INDEX CONCURRENTLY audit_a_id_idx"),
            "non-transactional segment should be audited; got {second_sql}"
        );

        for row in rows {
            let target_database: String = row
                .try_get("target_database")
                .expect("target_database column");
            let app_label: String = row.try_get("app_label").expect("app_label column");
            let sig: String = row
                .try_get("snapshot_signature_hex")
                .expect("snapshot signature column");
            let applied_before_next: Option<bool> = row
                .try_get("applied_before_next")
                .expect("applied_at monotonic column");

            assert_eq!(target_database, "main");
            assert_eq!(app_label, "");
            assert_eq!(sig, expected_sig);
            assert_eq!(
                sig,
                "0".repeat(64),
                "unset signing key should persist the no-op zero signature"
            );
            assert_ne!(
                applied_before_next,
                Some(false),
                "applied_at should be monotonic in audit id order"
            );
        }

        let temp_dir = std::env::temp_dir();
        if let Ok(temp_canon) = temp_dir.canonicalize()
            && cleanup_path.starts_with(&temp_canon)
        {
            let _ = std::fs::remove_file(&cleanup_path);
        }
    }

    #[djogi_test]
    async fn apply_plan_skips_audit_when_pool_none(mut ctx: DjogiContext) {
        let plan = single_table_plan("audit_pool_none_applies");
        let snapshot_path = unique_temp_path("audit-pool-none", "json");
        let cleanup_path = snapshot_path.clone();
        let runner_ctx = runner_ctx_for_audit_with_snapshot_path(&plan, None, Some(snapshot_path));
        let guard = acquire_test_workspace_guard();

        let report = apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect("audit_pool = None should not fail app-side apply");
        assert_eq!(report.transactional_segments, 1);

        let app_table: Option<String> = ctx
            .query_one(
                "SELECT to_regclass('public.audit_pool_none_applies')::text",
                &[],
            )
            .await
            .expect("query app table existence")
            .try_get(0)
            .expect("decode app table existence");
        assert_eq!(
            app_table.as_deref(),
            Some("audit_pool_none_applies"),
            "audit opt-out should still apply app-side DDL"
        );

        let audit_table: Option<String> = ctx
            .query_one("SELECT to_regclass('public.djogi_ddl_audit')::text", &[])
            .await
            .expect("query audit table existence")
            .try_get(0)
            .expect("decode audit table existence");
        assert_eq!(
            audit_table, None,
            "audit_pool = None should not bootstrap or write the audit table"
        );

        let temp_dir = std::env::temp_dir();
        if let Ok(temp_canon) = temp_dir.canonicalize()
            && cleanup_path.starts_with(&temp_canon)
        {
            let _ = std::fs::remove_file(&cleanup_path);
        }
    }

    #[test]
    fn audit_signature_for_unset_key_path_is_zero_hex() {
        let key = audit_signing_key_from_loaded(Ok(None));
        let sig_hex = audit_signature_hex_for_snapshot(&empty_snapshot(), key)
            .expect("render audit signature");

        assert_eq!(
            sig_hex,
            "0".repeat(64),
            "an unset signing key should use the no-op key and persist zero hex"
        );
    }

    /// #118 — apply path with `audit_pool: Some` AND
    /// `snapshot: None` (the `db reset` replay shape) MUST still
    /// write `djogi_ddl_audit` rows. Pre-fix the audit-write loop
    /// was gated on snapshot presence inside the same `if let`
    /// block as `save_snapshot`, so production reset (which passes
    /// `snapshot: None`) silently bypassed the audit overlay even
    /// though the pool was wired through.
    /// Distinguishing assertion: `snapshot_signature_hex` is `NULL`
    /// (not the no-op zero hex) — `NULL` is the contract for
    /// "no snapshot was supplied this apply".
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_plan_writes_audit_rows_when_snapshot_none(mut ctx: DjogiContext) {
        let _signing_key_env = SigningKeyEnvUnsetGuard::unset();
        let plan = single_table_plan("audit_snapshot_none_applies");
        let audit_pool = ctx
            .share_pool()
            .expect("djogi_test context should be pool-backed")
            .inner;
        // Build the runner ctx with audit_pool=Some BUT snapshot=None
        // this is the production `db reset` replay shape that the
        // pre-fix code path could not service.
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260509000001__reset_audit_test".to_string(),
            description: "snapshot-none audit test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: Some(audit_pool),
            runner_identity: Some(RunnerIdentity::SingleNodeDev), // test fixture — SingleNodeDev for non-P0 runner identity binding (G-gate requires it)
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let report = apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect("apply with audit_pool=Some and snapshot=None must succeed");
        assert_eq!(report.transactional_segments, 1);

        let row_count: i64 = ctx
            .query_one(
                "SELECT COUNT(*)::bigint FROM djogi_ddl_audit \
                 WHERE target_database = 'main' AND app_label = ''",
                &[],
            )
            .await
            .expect("count audit rows")
            .try_get(0)
            .expect("decode count");
        assert!(
            row_count >= 1,
            "expected at least one audit row when audit_pool=Some, even with snapshot=None; got {row_count}"
        );

        // Critical: signature column is NULL when no snapshot was
        // supplied. Distinguishes "no snapshot this apply" (NULL)
        // from "snapshot signed under no-op key" (zero hex). The
        // verify CLI's tolerant comparison treats NULL as a
        // no-stored-signature skip — see `verify::run`.
        let sig: Option<String> = ctx
            .query_one(
                "SELECT snapshot_signature_hex FROM djogi_ddl_audit \
                 WHERE target_database = 'main' AND app_label = '' \
                 ORDER BY id DESC LIMIT 1",
                &[],
            )
            .await
            .expect("read most recent audit row signature")
            .try_get(0)
            .expect("decode signature");
        assert_eq!(
            sig, None,
            "snapshot=None must persist NULL signature, not the no-op zero hex"
        );
    }

    #[djogi_test]
    async fn apply_plan_audit_failure_does_not_roll_back_app_db(mut ctx: DjogiContext) {
        let _signing_key_env = SigningKeyEnvReadGuard::hold();
        let plan = single_table_plan("audit_failure_survives");
        let snapshot_path = unique_temp_path("audit-failure-survives", "json");
        let cleanup_path = snapshot_path.clone();
        let audit_pool = crate::pg::pool::DjogiPool::builder(
            "postgres://djogi:djogi@127.0.0.1:1/djogi_unreachable",
        )
        .max_size(1)
        .timeout(Duration::from_millis(50))
        .build()
        .await
        .expect("build unreachable audit pool")
        .inner;
        let runner_ctx =
            runner_ctx_for_audit_with_snapshot_path(&plan, Some(audit_pool), Some(snapshot_path));
        let guard = acquire_test_workspace_guard();

        let report = apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect("audit-side failure should not fail app-side apply");
        assert_eq!(report.transactional_segments, 1);

        let app_table: Option<String> = ctx
            .query_one(
                "SELECT to_regclass('public.audit_failure_survives')::text",
                &[],
            )
            .await
            .expect("query app table existence")
            .try_get(0)
            .expect("decode app table existence");
        assert_eq!(
            app_table.as_deref(),
            Some("audit_failure_survives"),
            "app-side DDL should remain committed when the audit DB is unavailable"
        );

        let temp_dir = std::env::temp_dir();
        if let Ok(temp_canon) = temp_dir.canonicalize()
            && cleanup_path.starts_with(&temp_canon)
        {
            let _ = std::fs::remove_file(&cleanup_path);
        }
    }

    // ── RolledBack re-apply path ──────────────────────────────────────

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (PIN): Insert a RolledBack row to test the
    // classify_duplicate_version_collision function's handling of
    // non-terminal statuses. The public API has no insert_rolledback.
    #[djogi_test]
    async fn b1_rolledback_is_non_terminal_collision_class(mut ctx: DjogiContext) {
        // Verify that classify_duplicate_version_collision treats
        // RolledBack as non-terminal (VersionCollisionNonTerminal),
        // consistent with Decision 1 and the CLI's PendingOrPartial path.
        let version = "V20260526000001__b1_rolledback_test";

        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, status, run_id, \
              snapshot_version, app_label) \
             VALUES ($1, 'b1 test', 'V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855', \
                     'rolled_back', 99, '1.0', '')",
            &[&version],
        )
        .await
        .expect("insert RolledBack row");

        // Attempt to classify via the runner's collision path
        let result = classify_duplicate_version_collision(&mut ctx, version, "").await;
        match result {
            RunnerError::VersionCollisionNonTerminal { ref status, .. } => {
                assert!(
                    matches!(status, LedgerStatus::RolledBack),
                    "RolledBack should produce VersionCollisionNonTerminal, got {:?}",
                    status
                );
            }
            RunnerError::VersionAlreadyApplied { .. } => {
                panic!("RolledBack must NOT produce VersionAlreadyApplied (Decision 1)");
            }
            other => {
                panic!("unexpected error: {other:?}");
            }
        }
    }

    // ── regression guard: Pending status refusal ───────────────────────

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (PIN): Insert a Pending row to verify the
    // classify_duplicate_version_collision function distinguishes
    // Pending from Failed/RolledBack. The public API has no
    // insert_pending_row helper for test fixtures.
    #[djogi_test]
    async fn b1_regression_pending_row_not_auto_deleted(mut ctx: DjogiContext) {
        let version = "V20260526000002__b1_pending_guard";

        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, status, run_id, \
              snapshot_version, app_label) \
             VALUES ($1, 'b1 pending guard', 'V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855', \
                     'pending', 98, '1.0', '')",
            &[&version],
        )
        .await
        .expect("insert Pending row");

        let result = classify_duplicate_version_collision(&mut ctx, version, "").await;
        match result {
            RunnerError::VersionCollisionNonTerminal { ref status, .. } => {
                assert!(
                    matches!(status, LedgerStatus::Pending),
                    "Pending should produce VersionCollisionNonTerminal, got {:?}",
                    status
                );
            }
            RunnerError::VersionAlreadyApplied { .. } => {
                panic!("Pending must NOT produce VersionAlreadyApplied");
            }
            other => {
                panic!("unexpected error: {other:?}");
            }
        }
    }

    // ── C-1: fake_apply_inner out-of-order enforcement ────────────────────

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (PIN): Insert an Applied row to simulate an
    // existing higher version for the out-of-order detection test.
    #[djogi_test]
    async fn c1_fake_apply_rejects_ooo_with_reject_policy(mut ctx: DjogiContext) {
        let higher_version = "V20260527000000__c1_higher_peer";

        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, status, run_id, \
              snapshot_version, app_label) \
             VALUES ($1, 'c1 higher peer', 'V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855', \
                     'applied', 97, '1.0', '')",
            &[&higher_version],
        )
        .await
        .expect("insert higher applied row");

        let plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![],
            }],
        };
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260526000000__c1_lower_version".to_string(),
            description: "c1 lower test".to_string(),
            checksum_up: "V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig {
                concurrent_warn_relpages: 5000,
                strict_concurrent_warnings: false,
                pk_flip_long_tx_threshold_secs: 30,
                pk_flip_join_table_option: Default::default(),
            },
            out_of_order_policy: crate::migrate::OutOfOrderPolicy::Reject,
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::SingleNodeDev), // test fixture — SingleNodeDev for non-P0 runner identity binding (G-gate requires it)
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "test reason").await;

        assert!(
            matches!(result, Err(RunnerError::OutOfOrderRejected { .. })),
            "fake-apply with Reject policy and higher peer should return OutOfOrderRejected, got: {result:?}"
        );
    }

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (PIN): Insert an Applied row for OOO detection test
    // and read ledger row to verify out_of_order_flag.
    #[djogi_test]
    async fn c1_fake_apply_allows_ooo_with_diagnostic_and_sets_flag(mut ctx: DjogiContext) {
        let higher_version = "V20260527000001__c1_higher_peer2";

        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, status, run_id, \
              snapshot_version, app_label) \
             VALUES ($1, 'c1 higher peer 2', 'V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855', \
                     'applied', 96, '1.0', '')",
            &[&higher_version],
        )
        .await
        .expect("insert higher applied row");

        let plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![],
            }],
        };
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260526000001__c1_lower_version2".to_string(),
            description: "c1 lower test 2".to_string(),
            checksum_up: "V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig {
                concurrent_warn_relpages: 5000,
                strict_concurrent_warnings: false,
                pk_flip_long_tx_threshold_secs: 30,
                pk_flip_join_table_option: Default::default(),
            },
            out_of_order_policy: crate::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::SingleNodeDev), // test fixture — SingleNodeDev for non-P0 runner identity binding (G-gate requires it)
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "test reason").await;

        assert!(
            result.is_ok(),
            "fake-apply with AllowWithDiagnostic should succeed: {result:?}"
        );

        // Verify the ledger row has out_of_order_flag = true
        let rows = ctx
            .query_all(
                "SELECT out_of_order_flag FROM djogi_schema_migrations \
                 WHERE version = 'V20260526000001__c1_lower_version2'",
                &[],
            )
            .await
            .expect("query ledger");

        assert!(rows.len() == 1, "ledger row should exist");
        let ooo_flag: bool = rows[0].try_get(0).expect("get out_of_order_flag");
        assert!(
            ooo_flag,
            "out_of_order_flag should be true for OOO fake-apply"
        );
    }

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (PIN): Insert an Applied row for OOO note test
    // and read ledger row to verify note format.
    #[djogi_test]
    async fn c1_fake_apply_ooo_note_contains_override_reason(mut ctx: DjogiContext) {
        let higher_version = "V20260527000002__c1_higher_peer3";

        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");
        ctx.raw_execute(
            "INSERT INTO djogi_schema_migrations \
             (version, description, checksum_up, status, run_id, \
              snapshot_version, app_label) \
             VALUES ($1, 'c1 higher peer 3', 'V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855', \
                     'applied', 95, '1.0', '')",
            &[&higher_version],
        )
        .await
        .expect("insert higher applied row");

        let plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![],
            }],
        };
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260526000002__c1_lower_version3".to_string(),
            description: "c1 lower test 3".to_string(),
            checksum_up: "V1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig {
                concurrent_warn_relpages: 5000,
                strict_concurrent_warnings: false,
                pk_flip_long_tx_threshold_secs: 30,
                pk_flip_join_table_option: Default::default(),
            },
            out_of_order_policy: crate::migrate::OutOfOrderPolicy::AllowExplicit {
                override_reason: "merge window".to_string(),
            },
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::SingleNodeDev), // test fixture — SingleNodeDev for non-P0 runner identity binding (G-gate requires it)
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result =
            fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "schema pre-exists").await;

        assert!(
            result.is_ok(),
            "fake-apply with AllowExplicit should succeed: {result:?}"
        );

        // Verify the partial_apply_note contains both the reason and override
        let rows = ctx
            .query_all(
                "SELECT partial_apply_note FROM djogi_schema_migrations \
                 WHERE version = 'V20260526000002__c1_lower_version3'",
                &[],
            )
            .await
            .expect("query ledger");

        assert!(rows.len() == 1, "ledger row should exist");
        let note: String = rows[0].try_get(0).expect("get partial_apply_note");
        assert!(
            note.contains("faked at"),
            "note should contain 'faked at': {note}"
        );
        assert!(
            note.contains("reason: schema pre-exists"),
            "note should contain fake reason: {note}"
        );
        assert!(
            note.contains("out-of-order apply"),
            "note should contain out-of-order annotation: {note}"
        );
        assert!(
            note.contains("override: merge window"),
            "note should contain override reason: {note}"
        );
    }

    #[test]
    fn expand_partition_statement_partitioned_index_preserves_leaf_drop_down_sql() {
        let stmt = OperationSql {
            label: "PkFlipPartitionedIndex events_p".to_string(),
            up: "CREATE UNIQUE INDEX events_p_ts_id_desc_idx ON ONLY events_p (ts, id_desc);\n\
                 -- Per leaf: CREATE UNIQUE INDEX CONCURRENTLY <leaf>_ts_id_desc_idx\n"
                .to_string(),
            down: "DROP INDEX IF EXISTS events_p_ts_id_desc_idx;".to_string(),
            lossy: None,
        };

        let expanded = expand_partition_statement(
            &stmt,
            "events_p",
            &["events_p_a".to_string(), "events_p_b".to_string()],
            PartitionExpansionMode::ReplayStrict,
        )
        .expect("strict replay expansion with leaves");

        assert_eq!(
            expanded[0].label,
            "PkFlipPartitionedIndex events_p (parent-level)"
        );
        assert_eq!(
            expanded[0].down,
            "DROP INDEX IF EXISTS events_p_ts_id_desc_idx;"
        );
        // Leaf concurrent down: parent drop first (CASCADE for attached leaves),
        // then leaf drop (cleanup for unattached/partial-apply case).
        assert!(
            expanded.iter().any(|s| {
                s.label == "PkFlipPartitionedIndex events_p leaf=events_p_a (concurrent)"
                    && s.down
                        == "DROP INDEX IF EXISTS events_p_ts_id_desc_idx; \
                            DROP INDEX IF EXISTS events_p_a_ts_id_desc_idx"
            }),
            "leaf A concurrent must drop parent then leaf: {expanded:?}",
        );
        assert!(
            expanded.iter().any(|s| {
                s.label == "PkFlipPartitionedIndex events_p leaf=events_p_b (concurrent)"
                    && s.down
                        == "DROP INDEX IF EXISTS events_p_ts_id_desc_idx; \
                            DROP INDEX IF EXISTS events_p_b_ts_id_desc_idx"
            }),
            "leaf B concurrent must drop parent then leaf: {expanded:?}",
        );
        assert!(
            expanded
                .iter()
                .filter(|s| s.label.contains("(attach)"))
                .all(|s| s.down.is_empty()),
            "attach statements remain forward-only",
        );
    }

    #[test]
    fn expand_partition_statement_schema_qualified_leaf_in_drop_down_sql() {
        // When `lookup_partition_leaves` returns schema-qualified names
        // (e.g. "myschema.events_p_a"), CREATE INDEX must use the BARE
        // index name (Postgres forbids a schema on the index name), while
        // DROP INDEX and ATTACH PARTITION must re-qualify with the leaf's
        // schema. Regression test for GH #357 / #366.
        // The parent statement here matches the real emitter format from
        // pk_flip.rs: bare index name, schema-qualified `ON ONLY` target,
        // and the underscore column form `id_desc` (not the space form
        // `id DESC`).
        let parent = "myschema.events_p";
        let leaves = vec![
            "myschema.events_p_a".to_string(),
            "myschema.events_p_b".to_string(),
        ];

        let plan = OperationSql {
            label: format!("PkFlipPartitionedIndex {parent}"),
            up: "CREATE UNIQUE INDEX events_p_ts_id_desc_idx ON ONLY myschema.events_p (ts, id_desc)"
                .to_string(),
            down: "DROP INDEX IF EXISTS myschema.events_p_ts_id_desc_idx".to_string(),
            lossy: None,
        };

        let expanded = expand_partition_statement(
            &plan,
            parent,
            &leaves,
            PartitionExpansionMode::ApplyLenient,
        )
        .expect("partition expansion should succeed");

        // parent-level + (concurrent + attach) per leaf = 1 + 2*2 = 5.
        assert_eq!(expanded.len(), 5);

        // Leaf A concurrent: bare index name, schema-qualified ON target,
        // `_desc` suffix (no space), both DROP targets schema-qualified.
        let leaf_a_concurrent = expanded
            .iter()
            .find(|s| s.label == "PkFlipPartitionedIndex myschema.events_p leaf=myschema.events_p_a (concurrent)")
            .expect("leaf A concurrent entry");
        assert_eq!(
            leaf_a_concurrent.up,
            "CREATE UNIQUE INDEX CONCURRENTLY events_p_a_ts_id_desc_idx ON myschema.events_p_a (ts, id_desc)",
        );
        assert_eq!(
            leaf_a_concurrent.down,
            "DROP INDEX IF EXISTS myschema.events_p_ts_id_desc_idx; DROP INDEX IF EXISTS myschema.events_p_a_ts_id_desc_idx",
        );

        // Leaf A attach: both index names schema-qualified, no down.
        let leaf_a_attach = expanded
            .iter()
            .find(|s| {
                s.label
                    == "PkFlipPartitionedIndex myschema.events_p leaf=myschema.events_p_a (attach)"
            })
            .expect("leaf A attach entry");
        assert_eq!(
            leaf_a_attach.up,
            "ALTER INDEX myschema.events_p_ts_id_desc_idx ATTACH PARTITION myschema.events_p_a_ts_id_desc_idx",
        );
        assert_eq!(leaf_a_attach.down, "");

        // Invariant locks across every concurrent entry.
        for entry in expanded.iter().filter(|s| s.label.contains("(concurrent)")) {
            // The bug being guarded: a schema-qualified index name in the
            // CREATE INDEX CONCURRENTLY statement (Postgres syntax error).
            assert!(
                !entry.up.contains("CONCURRENTLY myschema."),
                "concurrent up must not have schema in index name: {}",
                entry.up,
            );
            // The other bug: the space column form `id DESC` instead of the
            // underscore form the emitter actually produces.
            assert!(
                !entry.up.contains("id DESC"),
                "up must not contain space-form 'id DESC': {}",
                entry.up,
            );
        }
    }

    #[test]
    fn expand_partition_leaf_placeholders_replay_mode_refuses_empty_leaves() {
        let stmt = OperationSql {
            label: "PkFlipPartitionedIndex events_p".to_string(),
            up: "CREATE UNIQUE INDEX events_p_ts_id_desc_idx ON ONLY events_p (ts, id_desc);"
                .to_string(),
            down: "DROP INDEX IF EXISTS events_p_ts_id_desc_idx;".to_string(),
            lossy: None,
        };

        let err = expand_partition_statement(
            &stmt,
            "events_p",
            &[],
            PartitionExpansionMode::ReplayStrict,
        )
        .expect_err("strict replay must not replace a partition expansion with a no-op comment");

        match err {
            RunnerError::PartitionExpansionNoLeaves {
                parent,
                statement_label,
            } => {
                assert_eq!(parent, "events_p");
                assert_eq!(statement_label, "PkFlipPartitionedIndex events_p");
            }
            other => panic!("expected PartitionExpansionNoLeaves, got {other:?}"),
        }

        let lenient = expand_partition_statement(
            &stmt,
            "events_p",
            &[],
            PartitionExpansionMode::ApplyLenient,
        )
        .expect("apply mode keeps the existing empty-leaf fallback");
        assert_eq!(lenient.len(), 1);
        assert!(lenient[0].up.contains("pg_inherits returned 0 leaves"));
    }

    #[test]
    fn rollback_leaf_identity_mismatch_display() {
        let err = RollbackError::LeafIdentityMismatch {
            version: "001_create_users".to_string(),
            stored_leaf_identity: "public.users:public.users_p2024_01,public.users_p2024_02\n"
                .to_string(),
            current_leaf_identity: "public.users:public.users_p2024_01,public.users_p2024_03\n"
                .to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("[D624]"));
        assert!(msg.contains("rollback refused"));
        assert!(msg.contains("partition leaf identity mismatch"));
        assert!(msg.contains("001_create_users"));
    }

    // ── Stage 4C: Rollback stale Phase 0 down and identity contract ─────

    #[test]
    fn rollback_stale_phase_zero_down_error_display() {
        let phase_zero_version = crate::migrate::bootstrap::PHASE_ZERO_VERSION;
        let err = RollbackError::StalePhaseZeroDown {
            version: phase_zero_version.to_string(),
            refusal_reason: "generated-stale",
        };
        let msg = format!("{}", err);
        assert!(msg.contains("rollback refused"));
        assert!(msg.contains("Phase 0 down SQL"));
        assert!(msg.contains("generated-stale"));
        assert!(msg.contains(phase_zero_version));
    }

    #[test]
    fn rollback_stale_phase_zero_down_error_display_ambiguous() {
        let phase_zero_version = crate::migrate::bootstrap::PHASE_ZERO_VERSION;
        let err = RollbackError::StalePhaseZeroDown {
            version: phase_zero_version.to_string(),
            refusal_reason: "ambiguous",
        };
        let msg = format!("{}", err);
        assert!(msg.contains("ambiguous"));
    }

    #[test]
    fn rollback_missing_identity_error_display() {
        let err = RollbackError::MissingRollbackIdentity {
            version: "V20260101000000__add_users".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("rollback refused"));
        assert!(msg.contains("non-Phase-0 rollback"));
        assert!(msg.contains("V20260101000000__add_users"));
        assert!(msg.contains("binding-capable runner identity"));
    }

    #[test]
    fn rollback_lossy_allow_reason_preserves_reason_without_lossy_markers() {
        let plan = single_table_plan("lossy_reason_markerless");
        let reason = "operator supplied a file-derived rollback reason".to_string();

        let allow_reason = rollback_lossy_allow_reason(
            &plan,
            &LossyRollbackPolicy::Allow {
                reason: reason.clone(),
            },
        )
        .expect("allow policy should not refuse markerless rollback");

        assert_eq!(
            allow_reason.as_deref(),
            Some(reason.as_str()),
            "Allow {{ reason }} must survive even when the replay plan has no lossy markers"
        );
    }

    #[test]
    fn rollback_error_live_db_committed_signal_is_conservative() {
        let pre_execution_runner = RollbackError::Runner {
            source: RunnerError::LockTimeout {
                path: PathBuf::from("/tmp/djogi-lock"),
                holder_pid: None,
            },
            live_db_committed: false,
        };
        assert!(
            !pre_execution_runner.live_db_committed(),
            "pre-execution runner refusals must report non-committed"
        );

        let post_commit_runner = RollbackError::Runner {
            source: RunnerError::LockTimeout {
                path: PathBuf::from("/tmp/djogi-lock"),
                holder_pid: None,
            },
            live_db_committed: true,
        };
        assert!(
            post_commit_runner.live_db_committed(),
            "post-commit runner errors must report committed"
        );

        let transactional_statement_failed = RollbackError::DownStatementFailed {
            segment_index: 0,
            statement_label: "DropTable tx".to_string(),
            live_db_committed: false,
            source: DjogiError::Db(DbError::other("transactional rollback failure")),
        };
        assert!(
            !transactional_statement_failed.live_db_committed(),
            "transactional statement failures before COMMIT should remain non-committed"
        );

        let non_transactional_statement_failed = RollbackError::DownStatementFailed {
            segment_index: 1,
            statement_label: "DropIndex concurrently".to_string(),
            live_db_committed: true,
            source: DjogiError::Db(DbError::other("non-transactional rollback failure")),
        };
        assert!(
            non_transactional_statement_failed.live_db_committed(),
            "non-transactional down failures must report committed once execution was attempted"
        );

        let snapshot_failed = RollbackError::SnapshotPersistFailed {
            path: PathBuf::from("/tmp/djogi-snapshot"),
            source: SnapshotError::Io {
                path: None,
                source: std::io::Error::other("snapshot write failed"),
            },
        };
        assert!(
            snapshot_failed.live_db_committed(),
            "snapshot persistence happens after down SQL and ledger mutation"
        );

        let checksum_refusal = RollbackError::ChecksumDrift {
            version: "V20260613000000__checksum_refusal".to_string(),
            side: RollbackChecksumSide::Up,
            ledger: Some(
                "V1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            ),
            on_disk: Some(
                "V1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            ),
        };
        assert!(
            !checksum_refusal.live_db_committed(),
            "checksum parity refusal must happen before destructive down SQL"
        );
    }

    #[test]
    fn migrate_surface_reexports_non_transactional_shape_helper() {
        assert_eq!(
            crate::migrate::find_non_transactional_statement_shape(
                "CREATE INDEX CONCURRENTLY idx ON table_name (id)"
            ),
            Some("CREATE INDEX CONCURRENTLY")
        );
    }

    #[test]
    fn phase_zero_down_classification_identifies_literal_database_default_as_stale() {
        // This is the generated-stale pattern: literal ALTER DATABASE with
        // hardcoded database name for HeeRanjID GUC defaults.
        let down_sql = "ALTER DATABASE \"mydb\" SET heer.node_id = '1';\n\
                        ALTER DATABASE \"mydb\" SET heer.ranj_node_id = '1';";
        let state = crate::migrate::phase_zero::classify_phase_zero_artifact(down_sql.as_bytes());
        // The artifact classifier looks at banner/section markers, so a bare
        // ALTER DATABASE without markers is classified as Ambiguous (not a
        // recognized generated artifact). This still triggers refusal.
        assert_ne!(
            state,
            crate::migrate::phase_zero::PhaseZeroArtifactState::IdentityFreeCurrent
        );
    }

    #[test]
    fn phase_zero_down_classification_identifies_complete_stale_as_generated_stale() {
        let mut down_sql =
            crate::migrate::bootstrap::compose_phase_zero("mydb", &BTreeSet::new(), 1, true)
                .expect("compose seed-capable Phase 0");
        let start = down_sql.find("DO $djogi$").expect("dynamic defaults");
        let end = down_sql[start..]
            .find("SET heer.node_id = '1';")
            .map(|offset| start + offset)
            .expect("session SET after dynamic defaults");
        down_sql.replace_range(
            start..end,
            "ALTER DATABASE \"mydb\" SET heer.node_id = '1';\n\
             ALTER DATABASE \"mydb\" SET heer.ranj_node_id = '1';\n",
        );
        let state = crate::migrate::phase_zero::classify_phase_zero_artifact(down_sql.as_bytes());
        assert_eq!(
            state,
            crate::migrate::phase_zero::PhaseZeroArtifactState::GeneratedStale
        );
    }

    #[test]
    fn phase_zero_down_classification_identity_free_production_is_ok() {
        // Production Phase 0 down: no node seed fragments.
        let down_sql = "DROP TABLE IF EXISTS heer.heer_nodes;\n\
                        DROP FUNCTION IF EXISTS heer.heerid_next();";
        let state = crate::migrate::phase_zero::classify_phase_zero_artifact(down_sql.as_bytes());
        // No generated markers → classified as Ambiguous (not a generated artifact).
        // This still triggers refusal in the rollback guard, which is correct:
        // the guard refuses anything that's not identity-free replay-current.
        assert_ne!(
            state,
            crate::migrate::phase_zero::PhaseZeroArtifactState::IdentityFreeCurrent
        );
    }

    #[test]
    fn phase_zero_down_classification_comment_only_is_not_current() {
        // Comment-only Phase 0 down has no generated markers, so the
        // rollback guard refuses it instead of treating it as a supported
        // rollback payload.
        let down_sql = "-- Phase 0 down — no reverse operations for bootstrap";
        let state = crate::migrate::phase_zero::classify_phase_zero_artifact(down_sql.as_bytes());
        // No generated markers → Ambiguous, which refuses in the guard.
        // This is correct: the rollback guard requires identity-free
        // replay-current classification.
        assert_ne!(
            state,
            crate::migrate::phase_zero::PhaseZeroArtifactState::IdentityFreeCurrent
        );
    }

    fn phase_zero_plan_with_down_payload(down: &str) -> MigrationPlan {
        MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![OperationSql {
                    label: "Phase 0 rollback payload".to_string(),
                    up: String::new(),
                    down: down.to_string(),
                    lossy: None,
                }],
            }],
        }
    }

    fn phase_zero_runner_ctx_for(plan: &MigrationPlan) -> RunnerCtx {
        RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "Phase 0 rollback preflight".to_string(),
            checksum_up: compute_checksum_for_plan_up(plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        }
    }

    #[test]
    fn phase_zero_down_missing_payload_refuses_before_execution() {
        for down in ["", " \n\t "] {
            let plan = phase_zero_plan_with_down_payload(down);
            let runner_ctx = phase_zero_runner_ctx_for(&plan);

            let result = preflight_phase_zero_down_payload(&plan, &runner_ctx);

            assert!(
                matches!(
                    result,
                    Err(RollbackError::StalePhaseZeroDown {
                        refusal_reason: "missing",
                        ..
                    })
                ),
                "missing/whitespace Phase 0 down payload must refuse, got {result:?}"
            );
        }
    }

    #[test]
    fn phase_zero_down_seed_capable_payload_refuses_before_execution() {
        let down_sql = crate::migrate::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            crate::migrate::bootstrap::DEFAULT_NODE_ID,
            true,
        )
        .expect("compose seed-capable Phase 0");
        let plan = phase_zero_plan_with_down_payload(&down_sql);
        let runner_ctx = phase_zero_runner_ctx_for(&plan);

        let result = preflight_phase_zero_down_payload(&plan, &runner_ctx);

        assert!(
            matches!(
                result,
                Err(RollbackError::StalePhaseZeroDown {
                    refusal_reason: "seed-capable-runtime-only",
                    ..
                })
            ),
            "seed-capable Phase 0 down payload must refuse before execution, got {result:?}"
        );
    }

    #[test]
    fn phase_zero_down_markerless_seed_payload_refuses_before_execution() {
        let mut down_sql = crate::migrate::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            crate::migrate::bootstrap::DEFAULT_NODE_ID,
            false,
        )
        .expect("compose identity-free Phase 0");
        down_sql.push_str("\nINSERT INTO heer.heer_nodes (id) VALUES (1);\n");
        let plan = phase_zero_plan_with_down_payload(&down_sql);
        let runner_ctx = phase_zero_runner_ctx_for(&plan);

        let result = preflight_phase_zero_down_payload(&plan, &runner_ctx);

        assert!(
            matches!(
                result,
                Err(RollbackError::StalePhaseZeroDown {
                    refusal_reason: "seed-dml-not-runtime-current",
                    ..
                })
            ),
            "markerless seed Phase 0 down payload must refuse before execution, got {result:?}"
        );
    }

    #[test]
    fn phase_zero_down_extended_seed_payloads_refuse_before_execution() {
        for (name, statement) in extended_seed_statement_cases() {
            let mut down_sql = crate::migrate::bootstrap::compose_phase_zero(
                "main",
                &BTreeSet::new(),
                crate::migrate::bootstrap::DEFAULT_NODE_ID,
                false,
            )
            .expect("compose identity-free Phase 0");
            down_sql.push('\n');
            down_sql.push_str(statement);
            down_sql.push('\n');
            let plan = phase_zero_plan_with_down_payload(&down_sql);
            let runner_ctx = phase_zero_runner_ctx_for(&plan);

            let result = preflight_phase_zero_down_payload(&plan, &runner_ctx);

            assert!(
                matches!(
                    result,
                    Err(RollbackError::StalePhaseZeroDown {
                        refusal_reason: "seed-dml-not-runtime-current",
                        ..
                    })
                ),
                "extended seed Phase 0 down payload {name} must refuse before execution, got {result:?}"
            );
        }
    }

    #[test]
    fn runner_identity_requires_binding_for_selected() {
        assert!(RunnerIdentity::Selected { id: 7 }.requires_binding());
    }

    #[test]
    fn runner_identity_requires_binding_for_single_node_dev() {
        assert!(RunnerIdentity::SingleNodeDev.requires_binding());
    }

    #[test]
    fn runner_identity_does_not_require_binding_for_identity_free() {
        assert!(!RunnerIdentity::IdentityFree.requires_binding());
    }

    #[test]
    fn runner_identity_selected_returns_node_id() {
        assert_eq!(RunnerIdentity::Selected { id: 7 }.node_id(), Some(7));
    }

    #[test]
    fn runner_identity_single_node_dev_returns_one() {
        assert_eq!(RunnerIdentity::SingleNodeDev.node_id(), Some(1));
    }

    #[test]
    fn runner_identity_free_returns_none() {
        assert_eq!(RunnerIdentity::IdentityFree.node_id(), None);
    }

    // ── Pre-pin validation ordering ──────────────────────────────────────

    #[tokio::test]
    async fn apply_plan_missing_identity_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_table_plan("pre_pin_apply_no_identity");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000000__pre_pin_apply_no_identity".to_string(),
            description: "pre-pin apply identity test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "missing identity must refuse before pin/connection checkout, got {result:?}"
        );
    }

    #[tokio::test]
    async fn fake_apply_plan_missing_identity_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_segment_plan(SegmentKind::Transactional, "fake", "SELECT 1");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000001__pre_pin_fake_no_identity".to_string(),
            description: "pre-pin fake identity test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "missing identity must refuse before fake-apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn baseline_plan_missing_identity_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_table_plan("pre_pin_baseline_no_identity");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000002__pre_pin_baseline_no_identity".to_string(),
            description: "pre-pin baseline identity test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = baseline_plan(&mut ctx, &plan.bucket, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "missing identity must refuse before baseline pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn rollback_plan_missing_identity_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_table_plan("pre_pin_rollback_no_identity");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000003__pre_pin_rollback_no_identity".to_string(),
            description: "pre-pin rollback identity test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = rollback_plan(
            &mut ctx,
            &plan,
            &runner_ctx,
            &guard,
            LossyRollbackPolicy::Refuse,
            None,
        )
        .await;

        assert!(
            matches!(result, Err(RollbackError::MissingRollbackIdentity { .. })),
            "missing identity must refuse before rollback pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn apply_plan_checksum_mismatch_precedes_missing_identity_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_table_plan("pre_pin_apply_bad_checksum");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000004__pre_pin_apply_bad_checksum".to_string(),
            description: "pre-pin apply checksum precedence test".to_string(),
            checksum_up: compute_checksum(["different sql"]),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(result, Err(RunnerError::ChecksumMismatch(_))),
            "checksum mismatch must precede missing identity and pin, got {result:?}"
        );
    }

    #[tokio::test]
    async fn fake_apply_plan_checksum_mismatch_precedes_missing_identity_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_segment_plan(SegmentKind::Transactional, "fake", "SELECT 1");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000005__pre_pin_fake_bad_checksum".to_string(),
            description: "pre-pin fake checksum precedence test".to_string(),
            checksum_up: compute_checksum(["different sql"]),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(result, Err(RunnerError::ChecksumMismatch(_))),
            "fake checksum mismatch must precede missing identity and pin, got {result:?}"
        );
    }

    #[tokio::test]
    async fn baseline_snapshot_refusal_precedes_missing_identity_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_table_plan("pre_pin_baseline_snapshot");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000006__pre_pin_baseline_snapshot".to_string(),
            description: "pre-pin baseline snapshot precedence test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = baseline_plan(&mut ctx, &plan.bucket, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(
                result,
                Err(RunnerError::BaselineSnapshotShouldNotBeProvided)
            ),
            "snapshot refusal must precede missing identity and pin, got {result:?}"
        );
    }

    #[tokio::test]
    async fn rollback_prior_snapshot_refusal_precedes_missing_identity_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = single_table_plan("pre_pin_rollback_prior_snapshot");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260602000007__pre_pin_rollback_prior_snapshot".to_string(),
            description: "pre-pin rollback prior snapshot precedence test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: Some(unique_temp_path("pre-pin-prior", "json")),
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = rollback_plan(
            &mut ctx,
            &plan,
            &runner_ctx,
            &guard,
            LossyRollbackPolicy::Refuse,
            None,
        )
        .await;

        assert!(
            matches!(result, Err(RollbackError::PriorSnapshotMissing)),
            "prior snapshot refusal must precede missing identity and pin, got {result:?}"
        );
    }

    #[tokio::test]
    async fn apply_plan_stale_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = generated_stale_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 stale artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "generated-stale",
                    ..
                })
            ),
            "stale Phase 0 artifact must refuse before apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn apply_plan_seed_capable_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = seed_capable_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 seed-capable artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "seed-capable-runtime-only",
                    ..
                })
            ),
            "seed-capable Phase 0 artifact must refuse before apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn apply_plan_markerless_seed_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = markerless_seed_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 markerless seed artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "seed-dml-not-runtime-current",
                    ..
                })
            ),
            "markerless seed Phase 0 artifact must refuse before apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn apply_plan_extended_seed_phase_zero_artifacts_refuse_before_pin() {
        for (name, statement) in extended_seed_statement_cases() {
            let mut ctx = unreachable_pool_context().await;
            let plan = phase_zero_plan_with_seed_statement(statement);
            let runner_ctx = RunnerCtx {
                bucket: plan.bucket.clone(),
                version: PHASE_ZERO_VERSION.to_string(),
                description: format!("pre-pin Phase 0 extended seed artifact test: {name}"),
                checksum_up: compute_checksum_for_plan_up(&plan),
                checksum_down: None,
                snapshot: Some(empty_snapshot()),
                snapshot_path: None,
                config: MigrateConfig::default(),
                out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
                audit_pool: None,
                runner_identity: None,
                drift_baseline: DriftBaseline::Disabled,
            };
            let guard = acquire_test_workspace_guard();

            let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

            assert!(
                matches!(
                    result,
                    Err(RunnerError::StalePhaseZeroArtifact {
                        refusal_reason: "seed-dml-not-runtime-current",
                        ..
                    })
                ),
                "extended seed Phase 0 artifact {name} must refuse before apply pins, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn fake_apply_plan_stale_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = generated_stale_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 fake stale artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "generated-stale",
                    ..
                })
            ),
            "stale Phase 0 artifact must refuse before fake-apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn fake_apply_plan_seed_capable_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = seed_capable_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 fake seed-capable artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "seed-capable-runtime-only",
                    ..
                })
            ),
            "seed-capable Phase 0 artifact must refuse before fake-apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn fake_apply_plan_markerless_seed_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = markerless_seed_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 fake markerless seed artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "seed-dml-not-runtime-current",
                    ..
                })
            ),
            "markerless seed Phase 0 artifact must refuse before fake-apply pins, got {result:?}"
        );
    }

    #[tokio::test]
    async fn fake_apply_plan_copy_from_seed_phase_zero_artifact_refuses_before_pin() {
        let mut ctx = unreachable_pool_context().await;
        let plan = phase_zero_plan_with_seed_statement(
            "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM STDIN;",
        );
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "pre-pin Phase 0 fake COPY FROM seed artifact test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None,
            drift_baseline: DriftBaseline::Disabled,
        };
        let guard = acquire_test_workspace_guard();

        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "pre-pin").await;

        assert!(
            matches!(
                result,
                Err(RunnerError::StalePhaseZeroArtifact {
                    refusal_reason: "seed-dml-not-runtime-current",
                    ..
                })
            ),
            "COPY FROM seed Phase 0 artifact must refuse before fake-apply pins, got {result:?}"
        );
    }

    #[test]
    fn truncate_for_log_handles_multibyte_boundary() {
        let sql = format!("{}{}", "é".repeat(200), " SELECT 1");
        let truncated = truncate_for_log(&sql);
        assert!(truncated.ends_with("..."));
        assert!(
            truncated.len() <= 259,
            "truncated message should keep the 256-byte budget plus ellipsis"
        );
    }

    // ── Class G: identity gate tests on apply/fake/baseline paths ──

    /// G-gate test: non-Phase-0 apply with `runner_identity: None` must
    /// refuse with `MissingApplyIdentity` before reaching `generate_run_id`.
    #[djogi_test]
    async fn apply_plan_no_identity_non_phase_zero_refused(mut ctx: DjogiContext) {
        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");

        let plan = single_table_plan("g_gate_apply_no_id");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260601000000__g_gate_apply".to_string(),
            description: "G-gate apply test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None, // intentionally missing — should be refused for non-P0
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "non-P0 apply with no identity should refuse with MissingApplyIdentity, got: {result:?}",
        );

        // Verify no ledger row was inserted (refusal is before insert).
        let count: i64 = ctx
            .query_one(
                "SELECT COUNT(*) FROM djogi_schema_migrations WHERE version = 'V20260601000000__g_gate_apply'",
                &[],
            )
            .await
            .expect("count ledger rows")
            .try_get(0)
            .expect("count column");
        assert_eq!(
            count, 0,
            "no ledger row should be inserted before the G-gate refusal"
        );
    }

    /// G-gate test: non-Phase-0 fake apply with `runner_identity: None` must
    /// refuse with `MissingApplyIdentity` before reaching `generate_run_id`.
    #[djogi_test]
    async fn fake_apply_no_identity_non_phase_zero_refused(mut ctx: DjogiContext) {
        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");

        let plan = single_segment_plan(SegmentKind::Transactional, "G-gate fake-apply", "SELECT 1");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260601000001__g_gate_fake".to_string(),
            description: "G-gate fake-apply test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None,
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None, // intentionally missing — should be refused for non-P0
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result = fake_apply_plan(&mut ctx, &plan, &runner_ctx, &guard, "G-gate test").await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "non-P0 fake-apply with no identity should refuse with MissingApplyIdentity, got: {result:?}",
        );

        // Verify no ledger row was inserted.
        let count: i64 = ctx
            .query_one(
                "SELECT COUNT(*) FROM djogi_schema_migrations WHERE version = 'V20260601000001__g_gate_fake'",
                &[],
            )
            .await
            .expect("count ledger rows")
            .try_get(0)
            .expect("count column");
        assert_eq!(
            count, 0,
            "no ledger row should be inserted before the G-gate refusal"
        );
    }

    /// G-gate test: non-Phase-0 baseline with `runner_identity: None` must
    /// refuse with `MissingApplyIdentity` before reaching `generate_run_id`.
    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (PIN): Create a table so the live-DB projection
    // succeeds and the test can reach the G-gate refusal point.
    #[djogi_test]
    async fn baseline_no_identity_non_phase_zero_refused(mut ctx: DjogiContext) {
        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");

        let plan = single_table_plan("g_gate_baseline_no_id");
        // Baseline needs a populated DB — create the table so projection succeeds.
        ctx.raw_execute(
            &format!("CREATE TABLE {} (id bigint)", "g_gate_baseline_no_id"),
            &[],
        )
        .await
        .expect("create table for baseline test");

        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260601000002__g_gate_baseline".to_string(),
            description: "G-gate baseline test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: None, // baseline requires None
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None, // intentionally missing — should be refused for non-P0
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let bucket = plan.bucket.clone();
        let result = baseline_plan(&mut ctx, &bucket, &runner_ctx, &guard, "G-gate test").await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "non-P0 baseline with no identity should refuse with MissingApplyIdentity, got: {result:?}",
        );

        // Verify no ledger row was inserted.
        let count: i64 = ctx
            .query_one(
                "SELECT COUNT(*) FROM djogi_schema_migrations WHERE version = 'V20260601000002__g_gate_baseline'",
                &[],
            )
            .await
            .expect("count ledger rows")
            .try_get(0)
            .expect("count column");
        assert_eq!(
            count, 0,
            "no ledger row should be inserted before the G-gate refusal"
        );
    }

    /// G-gate test: non-Phase-0 apply with `IdentityFree` must also refuse.
    #[djogi_test]
    async fn apply_plan_identity_free_non_phase_zero_refused(mut ctx: DjogiContext) {
        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");

        let plan = single_table_plan("g_gate_apply_idfree");
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: "V20260601000003__g_gate_idfree".to_string(),
            description: "G-gate IdentityFree test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::IdentityFree), // IdentityFree — should be refused for non-P0
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            matches!(result, Err(RunnerError::MissingApplyIdentity { .. })),
            "non-P0 apply with IdentityFree should refuse with MissingApplyIdentity, got: {result:?}",
        );

        // Verify no ledger row was inserted.
        let count: i64 = ctx
            .query_one(
                "SELECT COUNT(*) FROM djogi_schema_migrations WHERE version = 'V20260601000003__g_gate_idfree'",
                &[],
            )
            .await
            .expect("count ledger rows")
            .try_get(0)
            .expect("count column");
        assert_eq!(
            count, 0,
            "no ledger row should be inserted before the G-gate refusal"
        );
    }

    /// G-gate carve-out: Phase 0 apply with `runner_identity: None` must
    /// succeed — Phase 0 uses wall-clock run-id and is exempt from identity binding.
    #[djogi_test]
    async fn apply_phase_zero_no_identity_allowed(mut ctx: DjogiContext) {
        ledger::bootstrap(&mut ctx).await.expect("bootstrap ledger");

        let plan = current_phase_zero_plan();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: PHASE_ZERO_VERSION.to_string(),
            description: "G-gate Phase 0 carve-out test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: None, // Phase 0 — should NOT be refused
            drift_baseline: DriftBaseline::Disabled,
        };

        let guard = acquire_test_workspace_guard();
        let result = apply_plan(&mut ctx, &plan, &runner_ctx, &guard).await;

        assert!(
            result.is_ok(),
            "Phase 0 apply with no identity should succeed (carve-out), got: {result:?}",
        );

        // Verify the ledger row was inserted.
        let count: i64 = ctx
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM djogi_schema_migrations WHERE version = '{}'",
                    PHASE_ZERO_VERSION
                ),
                &[],
            )
            .await
            .expect("count ledger rows")
            .try_get(0)
            .expect("count column");
        assert_eq!(count, 1, "Phase 0 apply should insert a ledger row");
    }

    /// C1/L test: verify dual-GUC split produces distinct error messages.
    /// When the second GUC (set_heer_ranj_node_id) fails after the first
    /// succeeds, the error source must reference "bind heer.ranj_node_id"
    /// and NOT "bind heer.node_id".
    #[djogi_test]
    async fn bind_runner_node_identity_second_guc_failure(mut ctx: DjogiContext) {
        // The #[djogi_test] harness seeds node_id = 1 in heer_nodes.
        // We use node_id = 1 so the first GUC (set_heer_node_id) passes
        // its validation, then we replace the second GUC with a wrapper
        // that raises — exercising the dual-execute split in
        // bind_runner_node_identity.
        const TEST_NODE_ID: i32 = 1;

        // Replace set_heer_ranj_node_id(INTEGER) with a wrapper that raises,
        // so the second execute call fails after the first succeeds. The
        // HeeRanjID bootstrap installs functions in the current schema
        // (public), not a "heer" schema — the "heer." prefix is used only
        // for the GUC names (heer.node_id, heer.ranj_node_id).
        ctx.execute(
            "DROP FUNCTION IF EXISTS set_heer_ranj_node_id(INTEGER)",
            &[],
        )
        .await
        .expect("drop original second GUC function");
        ctx.execute(
            "CREATE FUNCTION set_heer_ranj_node_id(INTEGER) RETURNS void AS $$ \
             BEGIN RAISE EXCEPTION 'simulated second GUC failure'; END; $$ LANGUAGE plpgsql",
            &[],
        )
        .await
        .expect("create failing wrapper for second GUC");

        let result = super::bind_runner_node_identity(&mut ctx, TEST_NODE_ID).await;

        // The first GUC (set_heer_node_id) should succeed with node_id=1;
        // the second (our wrapper that raises) should fail. The error must
        // identify the SECOND call site, not the first.
        assert!(
            matches!(&result, Err(RunnerError::NodeIdentityBindingFailed { node_id, .. }) if *node_id == TEST_NODE_ID),
            "expected NodeIdentityBindingFailed for node_id={TEST_NODE_ID}, got: {result:?}",
        );

        let source = std::error::Error::source(&result.expect_err("should be error"))
            .expect("NodeIdentityBindingFailed should have a source")
            .to_string();
        assert!(
            source.contains("bind heer.ranj_node_id"),
            "error source should identify the second GUC, got: {source}",
        );
        // Verify it does NOT contain the first GUC label.
        assert!(
            !source.contains("bind heer.node_id"),
            "error source should not reference the first GUC (that one succeeded), got: {source}",
        );
    }

    // ── per-app version-stream tests ────────────────────────────────

    fn single_statement_plan_for_bucket(
        bucket_key: &BucketKey,
        version: &str,
        label: &str,
        sql: &str,
    ) -> (MigrationPlan, RunnerCtx) {
        let plan = MigrationPlan {
            bucket: bucket_key.clone(),
            classification: Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements: vec![op(label, sql)],
            }],
        };
        let runner_ctx = RunnerCtx {
            bucket: bucket_key.clone(),
            version: version.to_string(),
            description: "per-app version stream test".to_string(),
            checksum_up: compute_checksum_for_plan_up(&plan),
            checksum_down: None,
            snapshot: Some(empty_snapshot()),
            snapshot_path: None,
            config: MigrateConfig::default(),
            out_of_order_policy: crate::migrate::policy::OutOfOrderPolicy::AllowWithDiagnostic,
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::SingleNodeDev),
            drift_baseline: DriftBaseline::Disabled,
        };
        (plan, runner_ctx)
    }

    /// Two buckets in one database legitimately share a version string
    /// (compose stamps one version per run). Each (version, app_label)
    /// stream must apply independently.
    #[djogi_test]
    async fn same_version_applies_once_per_app_label(mut ctx: DjogiContext) {
        let guard = acquire_test_workspace_guard();

        let version = "V20260609000000__t397";
        let bucket_a = BucketKey {
            database: "main".into(),
            app: "users".into(),
        };
        let bucket_b = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };

        // Clean up any leftover tables from prior runs.
        let _ = ctx.execute("DROP TABLE IF EXISTS t397_users", &[]).await;
        let _ = ctx.execute("DROP TABLE IF EXISTS t397_system", &[]).await;

        let (plan_a, runner_ctx_a) = single_statement_plan_for_bucket(
            &bucket_a,
            version,
            "AddTable t397_users",
            "CREATE TABLE t397_users (id BIGINT PRIMARY KEY)",
        );
        let (plan_b, runner_ctx_b) = single_statement_plan_for_bucket(
            &bucket_b,
            version,
            "AddTable t397_system",
            "CREATE TABLE t397_system (id BIGINT PRIMARY KEY)",
        );

        apply_plan(&mut ctx, &plan_a, &runner_ctx_a, &guard)
            .await
            .expect("bucket A applies");

        // Before the per-app version-stream fix, this returns
        // Err(VersionAlreadyApplied) — bucket B is shadowed by
        // bucket A's ledger row at the same version.
        apply_plan(&mut ctx, &plan_b, &runner_ctx_b, &guard)
            .await
            .expect("bucket B must apply on its own (version, app_label) stream");

        let rows = super::ledger::select_all(&mut ctx)
            .await
            .expect("ledger readable");
        let same_version: Vec<_> = rows.iter().filter(|r| r.version == version).collect();
        assert_eq!(same_version.len(), 2, "one ledger row per app stream");
        assert!(same_version.iter().any(|r| r.app_label == "users"));
        assert!(same_version.iter().any(|r| r.app_label == "system"));
    }

    // ── apply-time drift gate: skip and proceed paths ──────────────────
    // These pin the gate's negative space — every input shape under which
    // the gate must NOT refuse. `DriftDetected` refusals are exercised at
    // the CLI layer; here we lock in: gate disabled, never-applied bucket
    // (Snapshot / Missing baselines), a fresh bucket in an otherwise
    // established database, and advisory-only (warning-severity) divergence.
    // Live tests (`#[djogi_test]`): a Postgres connection is required; they
    // compile here and run under the project's integration CI.

    /// A drifted live DB plus `DriftBaseline::Disabled` proceeds: the gate's
    /// outer arm short-circuits, so neither the history probe nor verify runs.
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_proceeds_with_gate_disabled(mut ctx: DjogiContext) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let first_table = format!("drift_disabled_a_{n}");
        let drift_table = format!("drift_disabled_extra_{n}");
        let second_table = format!("drift_disabled_b_{n}");
        let guard = acquire_test_workspace_guard();

        // First apply establishes applied history for the bucket.
        let first_plan = single_table_plan(&first_table);
        let first_ctx = runner_ctx_with_drift_baseline(
            &first_plan,
            "V20260613000010__drift_disabled_a",
            DriftBaseline::Disabled,
        );
        apply_plan(&mut ctx, &first_plan, &first_ctx, &guard)
            .await
            .expect("first apply establishes history");

        // Introduce out-of-band drift: an extra table the baseline would not
        // know about (a D602 error if the gate verified).
        ctx.batch_execute(&format!("CREATE TABLE {drift_table} (id bigint)"))
            .await
            .expect("create out-of-band drift table");

        // Second apply with the gate disabled must proceed despite the drift.
        let second_plan = single_table_plan(&second_table);
        let second_ctx = runner_ctx_with_drift_baseline(
            &second_plan,
            "V20260613000011__drift_disabled_b",
            DriftBaseline::Disabled,
        );
        apply_plan(&mut ctx, &second_plan, &second_ctx, &guard)
            .await
            .expect("apply with DriftBaseline::Disabled must proceed despite live drift");

        let created: Option<String> = ctx
            .query_one(
                &format!("SELECT to_regclass('public.{second_table}')::text"),
                &[],
            )
            .await
            .expect("regclass lookup")
            .try_get(0)
            .expect("regclass column");
        assert!(
            created.is_some(),
            "second migration table must exist after a gate-disabled apply"
        );
    }

    /// A never-applied bucket skips the gate even when a `Snapshot` baseline
    /// is supplied that would otherwise drift (the baseline names a table that
    /// does not exist in the fresh database → D601 if verified).
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_skips_drift_gate_on_never_applied_bucket(mut ctx: DjogiContext) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_skip_snapshot_{n}");
        let plan = single_table_plan(&table);
        let guard = acquire_test_workspace_guard();
        let runner_ctx = runner_ctx_with_drift_baseline(
            &plan,
            "V20260613000020__drift_skip_snapshot",
            DriftBaseline::Snapshot(drift_baseline_with_table("never_existed_table")),
        );

        apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect(
                "never-applied bucket must skip the gate even with a drifting Snapshot baseline",
            );

        let created: Option<String> = ctx
            .query_one(&format!("SELECT to_regclass('public.{table}')::text"), &[])
            .await
            .expect("regclass lookup")
            .try_get(0)
            .expect("regclass column");
        assert!(created.is_some(), "first migration table must be created");
    }

    /// A never-applied bucket skips the gate when the baseline is reported
    /// `Missing`: drift is undefined without a prior applied state.
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_skips_gate_for_missing_baseline_on_never_applied_bucket(mut ctx: DjogiContext) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_skip_missing_{n}");
        let plan = single_table_plan(&table);
        let guard = acquire_test_workspace_guard();
        let runner_ctx = runner_ctx_with_drift_baseline(
            &plan,
            "V20260613000030__drift_skip_missing",
            DriftBaseline::Missing,
        );

        apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect("never-applied bucket must skip the gate for a Missing baseline");
    }

    /// A never-applied bucket skips the gate when the baseline is reported
    /// `Corrupted`: drift is undefined without a prior applied state, so a
    /// first apply proceeds rather than refusing on an unreadable snapshot.
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_skips_drift_gate_on_never_applied_bucket_with_corrupted_baseline(
        mut ctx: DjogiContext,
    ) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_corrupted_skip_{n}");
        let plan = single_table_plan(&table);
        let guard = acquire_test_workspace_guard();
        let runner_ctx = runner_ctx_with_drift_baseline(
            &plan,
            "V20260613000070__drift_corrupted_skip",
            DriftBaseline::Corrupted("fake corruption error".to_string()),
        );
        apply_plan(&mut ctx, &plan, &runner_ctx, &guard)
            .await
            .expect("first apply with Corrupted baseline must succeed on never-applied bucket");
    }

    /// A brand-new bucket in an established database skips the gate: the
    /// history probe keys on the bucket's own app label, so applied history in
    /// a sibling bucket does not force the new bucket through verify. Covers
    /// both the `Snapshot` and `Missing` baselines for the new bucket.
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_skips_gate_for_new_bucket_in_established_database(mut ctx: DjogiContext) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let alpha_table = format!("drift_new_bucket_alpha_{n}");
        let beta_table = format!("drift_new_bucket_beta_{n}");
        let beta_table_missing = format!("drift_new_bucket_beta_missing_{n}");
        let guard = acquire_test_workspace_guard();

        // Establish applied history for bucket `alpha`.
        let alpha_plan = named_bucket_single_table_plan("djogi_test", "alpha", &alpha_table);
        let alpha_ctx = runner_ctx_with_drift_baseline(
            &alpha_plan,
            "V20260613000040__drift_new_bucket_alpha",
            DriftBaseline::Disabled,
        );
        apply_plan(&mut ctx, &alpha_plan, &alpha_ctx, &guard)
            .await
            .expect("alpha apply establishes history in the database");

        // First apply for the unrelated bucket `beta` with a drifting Snapshot
        // baseline must skip the gate — beta has no applied history of its own.
        let beta_plan = named_bucket_single_table_plan("djogi_test", "beta", &beta_table);
        let beta_ctx = runner_ctx_with_drift_baseline(
            &beta_plan,
            "V20260613000041__drift_new_bucket_beta",
            DriftBaseline::Snapshot(drift_baseline_with_table("never_existed_table")),
        );
        apply_plan(&mut ctx, &beta_plan, &beta_ctx, &guard)
            .await
            .expect("new bucket beta must skip the gate despite alpha's applied history");

        // The same fresh-bucket skip holds for a Missing baseline. Use a
        // distinct app so the bucket is again never-applied.
        let gamma_plan = named_bucket_single_table_plan("djogi_test", "gamma", &beta_table_missing);
        let gamma_ctx = runner_ctx_with_drift_baseline(
            &gamma_plan,
            "V20260613000042__drift_new_bucket_gamma",
            DriftBaseline::Missing,
        );
        apply_plan(&mut ctx, &gamma_plan, &gamma_ctx, &guard)
            .await
            .expect("new bucket gamma must skip the gate for a Missing baseline");
    }

    /// Advisory-only (warning-severity) divergence proceeds: an extra live
    /// index produces D611 (Warning), and the gate refuses only on
    /// error-severity drift (`report.has_errors()`).
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_proceeds_on_advisory_only_divergence(mut ctx: DjogiContext) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_advisory_{n}");
        let extra_index = format!("{table}_extra_idx");
        let second_table = format!("drift_advisory_b_{n}");
        let guard = acquire_test_workspace_guard();

        // Setup apply establishes the table and applied history.
        let setup_plan = single_table_plan(&table);
        let setup_ctx = runner_ctx_with_drift_baseline(
            &setup_plan,
            "V20260613000050__drift_advisory_setup",
            DriftBaseline::Disabled,
        );
        apply_plan(&mut ctx, &setup_plan, &setup_ctx, &guard)
            .await
            .expect("setup apply establishes the table");

        // Project the baseline from the live DB so it matches exactly; the
        // only divergence will be the extra index added next.
        let baseline =
            crate::migrate::verify::live_schema_for_repair(&mut ctx, &setup_plan.bucket, None)
                .await
                .expect("project baseline from live schema");

        // Add an out-of-band index not present in the baseline → D611 Warning.
        ctx.batch_execute(&format!("CREATE INDEX {extra_index} ON {table} (id)"))
            .await
            .expect("create out-of-band advisory index");

        // A new migration with the projected baseline must proceed: the only
        // diagnostic is a warning, so `has_errors()` is false.
        let next_plan = single_table_plan(&second_table);
        let next_ctx = runner_ctx_with_drift_baseline(
            &next_plan,
            "V20260613000051__drift_advisory_next",
            DriftBaseline::Snapshot(baseline),
        );
        apply_plan(&mut ctx, &next_plan, &next_ctx, &guard)
            .await
            .expect("advisory-only (warning) divergence must not refuse apply");

        let created: Option<String> = ctx
            .query_one(
                &format!("SELECT to_regclass('public.{second_table}')::text"),
                &[],
            )
            .await
            .expect("regclass lookup")
            .try_get(0)
            .expect("regclass column");
        assert!(
            created.is_some(),
            "second migration table must exist after an advisory-only proceed"
        );
    }

    /// Re-applying over committed partial non-transactional progress refuses
    /// as drift. A non-transactional segment autocommits each statement, so a
    /// later step's failure leaves the earlier step's table in the live
    /// catalog. On the next apply that committed-but-unrecorded table is drift
    /// the baseline does not describe (D602), and the gate refuses before any
    /// further SQL runs.
    #[allow(clippy::await_holding_lock)]
    #[djogi_test]
    async fn apply_over_partial_non_tx_progress_refuses_as_drift(mut ctx: DjogiContext) {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let setup_table = format!("drift_partial_setup_{n}");
        let committed_table = format!("drift_partial_committed_{n}");
        let broken_table = format!("drift_partial_broken_{n}");
        let guard = acquire_test_workspace_guard();

        // Establish the bucket so it has applied history.
        let setup_plan = single_table_plan(&setup_table);
        let setup_ctx = runner_ctx_with_drift_baseline(
            &setup_plan,
            "V20260613000060__drift_partial_setup",
            DriftBaseline::Disabled,
        );
        apply_plan(&mut ctx, &setup_plan, &setup_ctx, &guard)
            .await
            .expect("setup apply establishes the bucket");

        // Baseline projected now, before the partial progress lands. It does
        // not describe `committed_table`.
        let baseline =
            crate::migrate::verify::live_schema_for_repair(&mut ctx, &setup_plan.bucket, None)
                .await
                .expect("project baseline from live schema");

        // Two non-transactional segments: the first commits a table (autocommit),
        // the second is invalid SQL and fails — leaving the first table live.
        let partial_version = "V20260613000061__drift_partial_progress";
        let partial_plan = MigrationPlan {
            bucket: bucket("main", ""),
            classification: Classification::Additive,
            segments: vec![
                Segment {
                    kind: SegmentKind::NonTransactional,
                    statements: vec![op(
                        &format!("AddTable {committed_table}"),
                        &format!("CREATE TABLE {committed_table} (id bigint)"),
                    )],
                },
                Segment {
                    kind: SegmentKind::NonTransactional,
                    statements: vec![op(
                        &format!("AddTable {broken_table}"),
                        &format!("CREATE TABLE {broken_table} (id THIS_IS_NOT_A_TYPE)"),
                    )],
                },
            ],
        };
        let partial_ctx =
            runner_ctx_with_drift_baseline(&partial_plan, partial_version, DriftBaseline::Disabled);
        let partial_result = apply_plan(&mut ctx, &partial_plan, &partial_ctx, &guard).await;
        assert!(
            matches!(
                partial_result,
                Err(RunnerError::NonTransactionalSegmentFailed { .. })
            ),
            "second non-tx segment must fail, leaving partial progress, got: {partial_result:?}"
        );

        // The first non-tx statement committed its table out-of-band of the
        // baseline.
        let committed: Option<String> = ctx
            .query_one(
                &format!("SELECT to_regclass('public.{committed_table}')::text"),
                &[],
            )
            .await
            .expect("regclass lookup")
            .try_get(0)
            .expect("regclass column");
        assert!(
            committed.is_some(),
            "the first non-tx statement must have committed its table"
        );

        // Remove the failed ledger row so the same version can re-apply. The
        // bucket keeps its applied-history row from the setup migration.
        ctx.batch_execute(&format!(
            "DELETE FROM djogi_schema_migrations \
             WHERE version = '{partial_version}' AND app_label = ''"
        ))
        .await
        .expect("clear failed partial ledger row");

        // Re-applying with the pre-partial baseline must refuse: the committed
        // table is in the live catalog but not in the baseline (D602).
        let reapply_ctx = runner_ctx_with_drift_baseline(
            &partial_plan,
            partial_version,
            DriftBaseline::Snapshot(baseline),
        );
        let reapply_result = apply_plan(&mut ctx, &partial_plan, &reapply_ctx, &guard).await;
        assert!(
            matches!(reapply_result, Err(RunnerError::DriftDetected { .. })),
            "re-apply over committed partial non-tx progress must refuse as drift, got: {reapply_result:?}"
        );
    }
}
