//! Live-plan composition — helper functions and unit tests.
//!
//! This module defines the public entry points and data types used to
//! generate live-migration plan files from schema descriptor drift.
//! The compose pipeline diffs registered [`crate::descriptor::ModelDescriptor`]
//! instances against the persisted [`schema_snapshot.json`], classifies
//! each delta, and emits one or more plan files under
//! `migrations/<target_database>/live/`.

use crate::DjogiError;
use crate::context::DjogiContext;
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
    Serialize(PlanFileError),

    /// Database access error while checking for active plans or
    /// reading ledger state during compose.
    #[error(transparent)]
    Db(DjogiError),
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
    _ctx: &DjogiContext,
    _descriptors: Vec<crate::descriptor::ModelDescriptor>,
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
/// `(target_database, app_label)` pair. Format: `"target_db:app_label"`.
/// If the bucket contains no colon, it is treated as an app_label with
/// target_database defaulting to `"main"`.
///
/// If an active (non-terminal) plan row exists, returns
/// [`ComposeError::ActivePlanExists`] to prevent concurrent live-plan
/// execution on the same schema.
pub async fn check_no_active_plan(
    ctx: &mut DjogiContext,
    bucket: &str,
) -> Result<(), ComposeError> {
    let (target_db, app_label) = match bucket.split_once(':') {
        Some((db, label)) => (db.to_string(), label.to_string()),
        None => ("main".to_string(), bucket.to_string()),
    };

    let sql = "SELECT 1 FROM djogi_live_plans \
                WHERE target_database = $1 AND app_label = $2 \
                  AND status IN ('pending', 'running', 'paused')";
    let exists = ctx
        .query_opt(sql, &[&target_db, &app_label])
        .await
        .map_err(ComposeError::Db)?
        .is_some();

    if exists {
        return Err(ComposeError::ActivePlanExists);
    }
    Ok(())
}

/// Construct a minimal [`LivePlan`] skeleton from the supplied fields.
///
/// Populates the header with the given `plan_id`, `slug`, and
/// `classification`. The step list is used verbatim. Originating-migration
/// metadata is left at default values; the caller is responsible for
/// filling them before persisting.
///
/// # Panics
///
/// Panics if `classification` is [`crate::migrate::OnlineSafetyClassification::FastLockDestructiveGuarded`],
/// which does not route through the live-plan pipeline and should never
/// reach this function.
pub fn build_skeleton_plan(
    plan_id: String,
    slug: String,
    classification: crate::migrate::OnlineSafetyClassification,
    steps: Vec<crate::live_migrate::plan::Step>,
) -> crate::live_migrate::plan::LivePlan {
    use crate::live_migrate::plan::PlanClassification;

    let plan_id_val: crate::types::HeerId = plan_id
        .parse()
        .unwrap_or(crate::types::HeerId::ZERO);

    let plan_classification: PlanClassification =
        Option::<PlanClassification>::from(classification)
            .expect("FastLockDestructiveGuarded does not route through live-plan pipeline");

    crate::live_migrate::plan::LivePlan {
        header: crate::live_migrate::plan::PlanHeader {
            plan_id: plan_id_val,
            slug,
            classification: plan_classification,
            originating_migration: String::new(),
            target_database: String::new(),
            app_label: String::new(),
        },
        steps,
    }
}

/// Sanitise an app label for use in file paths and database columns.
///
/// Replaces non-alphanumeric characters (except ASCII hyphen and
/// underscore) with underscores. Collapses consecutive underscores to
/// one. Strips leading and trailing underscores or hyphens. Lowercases
/// the result. Truncates to 63 bytes (Postgres NAMEDATALEN limit).
/// Returns the empty string if input was empty or became empty after
/// sanitisation.
pub fn sanitize_app_label(label: &str) -> String {
    if label.is_empty() {
        return String::new();
    }

    // Replace non-alphanumeric (except hyphen, underscore) with underscore.
    let replaced: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Collapse multiple consecutive underscores to one.
    let collapsed: String =
        replaced
            .chars()
            .fold(String::with_capacity(replaced.len()), |mut acc, c| {
                if c == '_' && acc.ends_with('_') {
                    acc
                } else {
                    acc.push(c);
                    acc
                }
            });

    // Strip leading/trailing underscores or hyphens.
    let trimmed = collapsed.trim_matches(|c: char| c == '_' || c == '-');

    // Lowercase and truncate to 63 bytes.
    let lowercased = trimmed.to_lowercase();

    if lowercased.len() > 63 {
        // Safe: all characters are ASCII after sanitisation, so byte
        // boundaries align with character boundaries.
        let truncated = &lowercased[..63];
        truncated.to_string()
    } else {
        lowercased
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_migrate::plan::{Step, StepKind, StepParameters};

    // ── sanitize_app_label tests ───────────────────────────────────────

    #[test]
    fn sanitize_normal_input_preserved() {
        assert_eq!(sanitize_app_label("my_app"), "my_app");
    }

    #[test]
    fn sanitize_special_chars_replaced_with_underscore() {
        assert_eq!(sanitize_app_label("my@app#name"), "my_app_name");
    }

    #[test]
    fn sanitize_empty_input_returns_empty() {
        assert_eq!(sanitize_app_label(""), "");
    }

    #[test]
    fn sanitize_max_length_truncates_to_63_bytes() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_app_label(&long), "a".repeat(63));
    }

    #[test]
    fn sanitize_leading_separators_stripped() {
        assert_eq!(sanitize_app_label("__-my_app"), "my_app");
    }

    #[test]
    fn sanitize_trailing_separators_stripped() {
        assert_eq!(sanitize_app_label("my_app_-__"), "my_app");
    }

    #[test]
    fn sanitize_multiple_underscores_collapsed() {
        assert_eq!(sanitize_app_label("a___b"), "a_b");
    }

    #[test]
    fn sanitize_lowercase_applied() {
        assert_eq!(sanitize_app_label("MyApp"), "myapp");
    }

    #[test]
    fn sanitize_hyphens_preserved_in_middle() {
        assert_eq!(sanitize_app_label("my-app_name"), "my-app_name");
    }

    #[test]
    fn sanitize_only_special_chars_returns_empty() {
        assert_eq!(sanitize_app_label("@#$%^&*()"), "");
    }

    // ── build_skeleton_plan tests ──────────────────────────────────────

    fn dummy_step(ordinal: u32) -> Step {
        Step {
            kind: StepKind::ExpandSchema,
            ordinal,
            parameters: StepParameters::ExpandSchema {
                sql_segments: vec!["ALTER TABLE foo ADD COLUMN bar INT".to_string()],
            },
        }
    }

    #[test]
    fn build_skeleton_plan_constructs_valid_plan() {
        let plan = build_skeleton_plan(
            "123".to_string(),
            "add_bar_column".to_string(),
            crate::migrate::OnlineSafetyClassification::ExpandContract,
            vec![dummy_step(0), dummy_step(1), dummy_step(2)],
        );
        assert_eq!(plan.header.slug, "add_bar_column");
        assert_eq!(plan.steps.len(), 3);
    }

    #[test]
    fn build_skeleton_plan_parses_plan_id_from_string() {
        let plan = build_skeleton_plan(
            "42".to_string(),
            "test_slug".to_string(),
            crate::migrate::OnlineSafetyClassification::ExpandContract,
            vec![dummy_step(0)],
        );
        assert_eq!(plan.header.plan_id.as_i64(), 42);
    }
}
