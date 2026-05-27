//! Live-plan composition — type stubs for the compose pipeline.
//!
//! This module defines the public entry points and data types used to
//! generate live-migration plan files from schema descriptor drift.
//! The compose pipeline diffs registered [`crate::model::ModelDescriptor`]
//! instances against the persisted [`schema_snapshot.json`], classifies
//! each delta, and emits one or more plan files under
//! `migrations/<target_database>/live/`.
//!
//! # Stage 2A — type stubs only
//!
//! All public functions below carry `todo!()` bodies. Implementation
//! logic lands in subsequent stages; this file exists so the module
//! is addressable from mod.rs and downstream imports resolve during
//! incremental development.

use crate::error::DbError;
use crate::live_migrate::plan_file::PlanFileError;

// ── ComposeError ───────────────────────────────────────────────────────

/// Errors raised by the compose pipeline.
///
/// Each variant maps to a single failure mode in the plan generation
/// flow: schema diff, classification refusal, concurrency guard,
/// I/O, serialisation, or database access. The enum is non-exhaustive
/// so future stages can add variants (e.g., checksum conflicts, bucket
/// mismatch) without breaking downstream matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeError {
    /// The schema diff step produced an actionable error message.
    #[error("schema diff failed: {0}")]
    Diff(String),

    /// A delta classified as `OfflineOnly` cannot be composed into a
    /// live plan. The operator must handle the change manually.
    #[error("offline-only operation cannot be composed as live plan")]
    OfflineOnly,

    /// A live plan is already active in the target bucket. Compose
    /// refuses to generate a second concurrent plan for the same
    /// `(target_database, app_label)` pair.
    #[error("active plan already exists in bucket")]
    ActivePlanExists,

    /// Underlying file-system I/O failure during directory creation
    /// or plan-file write.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Serialising a [`crate::live_migrate::plan::LivePlan`] to JSON
    /// failed. Should not occur with well-formed plans; surfaced for
    /// completeness.
    #[error(transparent)]
    Serialize(#[from] PlanFileError),

    /// Database access error while checking for active plans or
    /// reading ledger state during compose.
    #[error(transparent)]
    Db(#[from] DbError),
}

// ── ExtractResult ──────────────────────────────────────────────────────

/// Outcome of inspecting a step list for backfill parameters.
///
/// Used by the runner to locate the [`StepKind::BackfillChunked`](
/// crate::live_migrate::plan::StepKind::BackfillChunked) step in a plan
/// and extract its table, predicate, column list, and batch size so
/// the backfill executor can drive chunks without re-parsing the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractResult {
    /// Backfill parameters extracted successfully from a
    /// `BackfillChunked` step in the plan.
    Params {
        /// Target table for the backfill UPDATE statements.
        table: String,
        /// SQL predicate fragment (the idempotent WHERE clause).
        filter: String,
        /// Columns that the backfill touches — used by the executor
        /// to scope the RETURNING clause and audit trail.
        columns: Vec<String>,
        /// Number of rows per chunk transaction.
        batch_size: i64,
    },
    /// The step list contains no `BackfillChunked` step. The plan is
    /// a non-backfill rollout (e.g., pure DDL or cutover-only).
    NotBackfillChunked,
    /// A `BackfillChunked` step was found but its parameters could not
    /// be decoded into the expected shape. Carries the diagnostic
    /// message for operator-facing rendering.
    Malformed(String),
}

// ── ComposeMeta ────────────────────────────────────────────────────────

/// Metadata supplied by the CLI or library caller to the compose
/// pipeline. Each field is required — the compiler enforces presence
/// via the struct's public fields rather than a builder pattern.
#[derive(Debug, Clone)]
pub struct ComposeMeta {
    /// Root of the project workspace (contains `migrations/`,
    /// `Cargo.toml`, etc.). Used to resolve plan-file output paths
    /// relative to `migrations/<target_database>/live/`.
    pub workspace_root: std::path::PathBuf,
    /// Phase 7 migration version that triggered this compose run.
    /// Recorded in the plan header's `originating_migration` field.
    pub originating_migration: String,
    /// Which of the three Djogi databases this plan targets
    /// (`main`, `crud_log`, `event_log`). Defaults to `main` at the
    /// CLI layer but must be explicit here.
    pub target_database: String,
    /// App label for multi-app isolation. Empty string for the
    /// synthetic global bucket.
    pub app_label: String,
}

// ── ComposeReport ──────────────────────────────────────────────────────

/// Summary returned by [`compose_live_plans`] after processing all
/// supplied descriptors. The caller renders this to the terminal or
/// threads it into the CLI's exit-code logic.
#[derive(Debug, Default)]
pub struct ComposeReport {
    /// Number of plan files successfully composed and written to disk.
    pub plans_composed: usize,
    /// Absolute paths to the on-disk plan JSON files. Ordered to match
    /// [`ComposeReport::plan_ids`].
    pub plan_file_paths: Vec<std::path::PathBuf>,
    /// HeerId primary keys for each composed plan. Ordered to match
    /// [`ComposeReport::plan_file_paths`].
    pub plan_ids: Vec<String>,
}

// ── StepResult ─────────────────────────────────────────────────────────

/// Outcome of executing or checking a single step in a live plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    /// The step completed without interruption.
    Completed,
    /// A chunked backfill step completed partially — some rows were
    /// processed but the predicate has not yet exhausted. Carries
    /// progress counters so the operator can render a percentage.
    Partial {
        /// Rows processed so far in this backfill range.
        rows_done: i64,
        /// Estimated total rows in the backfill range. May be zero
        /// if the estimate was unavailable at start time.
        rows_total: i64,
    },
    /// The step is paused at an operator gate (e.g., validation or
    /// cutover). The runner will not advance until the operator
    /// explicitly resumes via the CLI.
    Paused,
}

// ── Public functions ───────────────────────────────────────────────────

/// Compose live-migration plan files from schema descriptor drift.
///
/// Walks `descriptors`, diffs each against the snapshot at
/// `snapshot_path`, classifies the resulting deltas, and emits plan
/// files for any delta that routes through the live-plan pipeline
/// (i.e., [`OnlineSafetyClassification::ExpandContract`](
/// crate::migrate::OnlineSafetyClassification::ExpandContract)).
///
/// Returns a [`ComposeReport`] summarising what was written. Errors
/// via [`ComposeError`] on diff failure, classification refusal, or
/// I/O issues.
pub async fn compose_live_plans(
    _ctx: &crate::DjogiContext,
    _descriptors: Vec<crate::model::ModelDescriptor>,
    _snapshot_path: std::path::PathBuf,
    _meta: ComposeMeta,
) -> Result<ComposeReport, ComposeError> {
    todo!("compose_live_plans: Stage 2A stub — implementation in later stage")
}

/// Inspect a plan's step list and extract backfill parameters if a
/// [`StepKind::BackfillChunked`](crate::live_migrate::plan::StepKind::BackfillChunked)
/// step is present.
///
/// Returns [`ExtractResult::Params`] with the table, filter predicate,
/// column list, and batch size when the step is well-formed. Returns
/// [`ExtractResult::NotBackfillChunked`] if no matching step exists,
/// or [`ExtractResult::Malformed`] if the step's parameters cannot be
/// decoded into the expected shape.
pub fn extract_backfill_params(_steps: &[crate::live_migrate::plan::Step]) -> ExtractResult {
    todo!("extract_backfill_params: Stage 2A stub — implementation in later stage")
}

/// Verify that no live plan is currently active in the given bucket.
///
/// The bucket is identified by a single string key derived from the
/// `(target_database, app_label)` pair. If an active (non-terminal)
/// plan row exists, returns [`ComposeError::ActivePlanExists`] to
/// prevent concurrent live-plan execution on the same schema.
pub async fn check_no_active_plan(
    _ctx: &crate::DjogiContext,
    _bucket: &str,
) -> Result<(), ComposeError> {
    todo!("check_no_active_plan: Stage 2A stub — implementation in later stage")
}

/// Construct a minimal [`LivePlan`] skeleton from the supplied fields.
///
/// Populates the header with the given `plan_id`, `slug`, and
/// `classification`. The step list is used verbatim. Timestamps and
/// originating-migration metadata are left at default values; the
/// caller is responsible for filling them before persisting.
pub fn build_skeleton_plan(
    _plan_id: String,
    _slug: String,
    _classification: crate::migrate::OnlineSafetyClassification,
    _steps: Vec<crate::live_migrate::plan::Step>,
) -> crate::live_migrate::plan::LivePlan {
    todo!("build_skeleton_plan: Stage 2A stub — implementation in later stage")
}

/// Sanitise an app label for use in file paths and database columns.
///
/// Replaces any byte that is not an ASCII letter, ASCII digit, or
/// underscore with an underscore. Truncates the result to 63 bytes
/// (Postgres's NAMEDATALEN limit). Returns the empty string if input
/// was empty or became empty after sanitisation.
pub fn sanitize_app_label(_label: &str) -> String {
    todo!("sanitize_app_label: Stage 2A stub — implementation in later stage")
}
