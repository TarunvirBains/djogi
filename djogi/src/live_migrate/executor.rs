//! Live-plan execution engine.
//!
//! Reads a [`LivePlan`] from disk, executes each step sequentially,
//! and tracks progress in the `djogi_live_plans` ledger via the
//! state helpers from [`super::state`].

use crate::context::DjogiContext;
use crate::live_migrate::compose::StepResult;
use crate::live_migrate::plan::{LivePlan, Step, StepKind};

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
    pub is_resume: bool,
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
    ctx: &mut DjogiContext,
    plan_path: std::path::PathBuf,
    start_index: u32,
    is_resume: bool,
) -> Result<StepResult, ExecutorError> {
    // 1. Load plan from disk
    let plan = crate::live_migrate::plan_file::read_plan(&plan_path)?;

    // 2. Check for active plan conflicts in the same bucket
    let bucket = format!("{}:{}", plan.header.target_database, plan.header.app_label);
    crate::live_migrate::compose::check_no_active_plan(ctx, &bucket)
        .await
        .map_err(|e| ExecutorError::AlreadyCompleted(e.to_string()))?;

    // 3. Execute each step sequentially
    let mut last_result = StepResult::Completed;

    for (idx, step) in plan.steps.iter().enumerate() {
        if (idx as u32) < start_index {
            continue; // Skip already-completed steps on resume
        }
        let exec = ExecutionContext {
            ctx,
            plan: &plan,
            current_step: step,
            step_ordinal: step.ordinal,
            is_resume,
        };

        match execute_step(exec).await {
            Ok(result) => {
                last_result = result;

                // If paused at an operator gate, stop execution
                if matches!(last_result, StepResult::Paused) {
                    return Ok(StepResult::Paused);
                }

                // If partial backfill, stop and return progress
                if matches!(last_result, StepResult::Partial { .. }) {
                    return Ok(last_result);
                }
            }
            Err(e) => {
                // Record failure in ledger
                let error_msg = e.to_string();
                super::state::record_failure(
                    ctx,
                    plan.header.plan_id,
                    &plan.header.target_database,
                    &plan.header.app_label,
                    error_msg,
                    false, // not retriable by default
                )
                .await
                .map_err(ExecutorError::Db)?;

                return Err(e);
            }
        }
    }

    Ok(last_result)
}

/// Execute a single step based on its kind.
///
/// Dispatches to the appropriate handler for each [`StepKind`].
/// Non-destructive steps execute immediately; destructive or gated
/// steps may return early with [`StepResult::Paused`].
pub async fn execute_step(exec: ExecutionContext<'_>) -> Result<StepResult, ExecutorError> {
    match exec.current_step.kind {
        // DDL steps — expand, finalize, cleanup
        StepKind::ExpandSchema
        | StepKind::FinalizeConstraints
        | StepKind::CleanupLegacyState
        | StepKind::RunReversibleSchemaOp => execute_ddl_step(exec).await,

        // Backfill step — chunked data migration
        StepKind::BackfillChunked => execute_backfill_step(exec).await,

        // Compatibility window — no-op marker (hooks registered elsewhere)
        StepKind::BeginCompatibilityWindow => Ok(StepResult::Completed),

        // Operator gates — pause and wait for explicit resume
        StepKind::ValidateBackfill | StepKind::CutoverReads | StepKind::CutoverWrites => {
            handle_operator_gate(exec).await
        }
    }
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
    exec: ExecutionContext<'_>,
) -> Result<StepResult, ExecutorError> {
    use crate::live_migrate::backfill::{BackfillError, execute_backfill, resume_backfill};
    use crate::live_migrate::plan::StepParameters;

    let StepParameters::BackfillChunked {
        table,
        predicate_template,
        chunk_size,
    } = &exec.current_step.parameters
    else {
        return Err(ExecutorError::StepFailed {
            ordinal: exec.step_ordinal,
            reason: format!(
                "expected BackfillChunked parameters, got {:?}",
                exec.current_step.kind
            ),
        });
    };

    let _chunks = if exec.is_resume {
        resume_backfill(
            exec.ctx,
            exec.plan.header.plan_id,
            table,
            predicate_template,
            *chunk_size,
            false, // emit_events — suppression default per module docs
        )
        .await
    } else {
        execute_backfill(
            exec.ctx,
            exec.plan.header.plan_id,
            table,
            predicate_template,
            *chunk_size,
            false, // emit_events — suppression default per module docs
        )
        .await
    }
    .map_err(|e| match e {
        BackfillError::Database(db_err) => ExecutorError::Db(db_err),
        other => ExecutorError::StepFailed {
            ordinal: exec.step_ordinal,
            reason: other.to_string(),
        },
    })?;

    Ok(StepResult::Completed)
}

/// Execute a DDL step (ExpandSchema, FinalizeConstraints, etc.).
///
/// Runs each SQL segment within a single transaction. On failure,
/// the entire step is rolled back and recorded via [`super::state::record_failure`].
pub async fn execute_ddl_step(exec: ExecutionContext<'_>) -> Result<StepResult, ExecutorError> {
    use crate::live_migrate::plan::StepParameters;

    // Extract SQL segments from step parameters based on kind
    let sql_segments = match &exec.current_step.parameters {
        StepParameters::ExpandSchema { sql_segments } => sql_segments.clone(),
        StepParameters::FinalizeConstraints { sql_segments } => sql_segments.clone(),
        StepParameters::CleanupLegacyState { sql_segments } => sql_segments.clone(),
        StepParameters::RunReversibleSchemaOp { up_sql, .. } => vec![up_sql.clone()],
        _ => {
            return Err(ExecutorError::StepFailed {
                ordinal: exec.step_ordinal,
                reason: format!("expected DDL parameters, got {:?}", exec.current_step.kind),
            });
        }
    };

    // Execute all segments within a single transaction. On failure,
    // the entire step rolls back so no partial DDL is committed.
    crate::transaction::atomic(exec.ctx, |tx_ctx| {
        let segments = sql_segments.clone();
        Box::pin(async move {
            for segment in &segments {
                tx_ctx.execute(segment, &[]).await?;
            }
            Ok::<_, crate::DjogiError>(StepResult::Completed)
        })
    })
    .await
    .map_err(ExecutorError::Db)
}

/// Handle an operator gate step (ValidateBackfill, CutoverReads, CutoverWrites).
///
/// Pauses execution and returns [`StepResult::Paused`]. The operator
/// must explicitly resume via the CLI before the next step executes.
pub async fn handle_operator_gate(exec: ExecutionContext<'_>) -> Result<StepResult, ExecutorError> {
    use crate::live_migrate::state::{PlanStatus, update_status};

    // Map step kind to appropriate paused status
    let status = match exec.current_step.kind {
        crate::live_migrate::plan::StepKind::ValidateBackfill => PlanStatus::Validating,
        crate::live_migrate::plan::StepKind::CutoverReads
        | crate::live_migrate::plan::StepKind::CutoverWrites => PlanStatus::Cutover,
        _ => PlanStatus::Paused,
    };

    // Update ledger to reflect gate state
    update_status(
        exec.ctx,
        exec.plan.header.plan_id,
        &exec.plan.header.target_database,
        &exec.plan.header.app_label,
        status,
    )
    .await
    .map_err(ExecutorError::Db)?;

    Ok(StepResult::Paused)
}
