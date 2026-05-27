//! Live-plan execution engine.
//!
//! Reads a [`LivePlan`] from disk, executes each step sequentially,
//! and tracks progress in the `djogi_live_plans` ledger via the
//! state helpers from [`super::state`].

use crate::context::DjogiContext;
use crate::live_migrate::compose::StepResult;
use crate::live_migrate::plan::{LivePlan, Step};

// ── ExecutorError ────────────────────────────────────────────────────────

/// Errors that can occur during live-plan execution.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutorError {
    #[error("plan file error: {0}")]
    PlanFile(#[from] crate::live_migrate::plan_file::PlanFileError),

    #[error("database error: {0}")]
    Db(crate::DjogiError),

    #[error("step {ordinal} failed: {reason}")]
    StepFailed { ordinal: u32, reason: String },

    #[error("backfill concurrency conflict: {0}")]
    ConcurrencyConflict(String),

    #[error("plan already completed with status: {0}")]
    AlreadyCompleted(String),

    #[error("step {ordinal} requires operator gate")]
    OperatorGate { ordinal: u32, step_kind: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ── ExecutionContext ─────────────────────────────────────────────────────

/// Execution context passed to each step handler.
pub struct ExecutionContext<'a> {
    pub ctx: &'a mut DjogiContext,
    pub plan: &'a LivePlan,
    pub current_step: &'a Step,
    pub step_ordinal: u32,
}

// ── Public functions ─────────────────────────────────────────────────────

/// Run a live plan from the given path.
///
/// Loads the plan, verifies no active plan conflicts exist, then
/// executes each step sequentially. Progress is tracked in the
/// `djogi_live_plans` ledger.
///
/// Returns [`StepResult::Completed`] when all steps finish,
/// [`StepResult::Partial`] if a backfill was interrupted,
/// or [`StepResult::Paused`] if an operator gate was reached.
pub async fn run_plan(
    _ctx: &mut DjogiContext,
    _plan_path: std::path::PathBuf,
) -> Result<StepResult, ExecutorError> {
    todo!("run_plan: Stage 3B implementation pending")
}

/// Execute a single step based on its kind.
///
/// Dispatches to the appropriate handler for each [`StepKind`].
/// Non-destructive steps execute immediately; destructive or gated
/// steps may return early with [`StepResult::Paused`].
pub async fn execute_step(_exec: ExecutionContext<'_>) -> Result<StepResult, ExecutorError> {
    todo!("execute_step: Stage 3C implementation pending")
}

/// Execute a chunked backfill step.
///
/// Processes rows in batches of `chunk_size`, committing each batch
/// as a separate transaction. Progress is tracked via the ledger so
/// the backfill can be resumed after interruption.
///
/// Returns [`StepResult::Completed`] when all rows are processed,
/// [`StepResult::Partial`] with progress counters if interrupted.
pub async fn execute_backfill_step(
    _exec: ExecutionContext<'_>,
) -> Result<StepResult, ExecutorError> {
    todo!("execute_backfill_step: Stage 3D implementation pending")
}

/// Execute a DDL step (ExpandSchema, FinalizeConstraints, etc.).
///
/// Runs each SQL segment within a single transaction. On failure,
/// the entire step is rolled back and recorded via [`super::state::record_failure`].
pub async fn execute_ddl_step(_exec: ExecutionContext<'_>) -> Result<StepResult, ExecutorError> {
    todo!("execute_ddl_step: Stage 3E implementation pending")
}

/// Handle an operator gate step (ValidateBackfill, CutoverReads, CutoverWrites).
///
/// Pauses execution and returns [`StepResult::Paused`]. The operator
/// must explicitly resume via the CLI before the next step executes.
pub async fn handle_operator_gate(
    _exec: ExecutionContext<'_>,
) -> Result<StepResult, ExecutorError> {
    todo!("handle_operator_gate: Stage 3F implementation pending")
}
