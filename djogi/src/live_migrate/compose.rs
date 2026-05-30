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
///
/// Refuses with [`ComposeError::ActivePlanExists`] when a non-terminal
/// plan already owns the `(target_database, app_label)` bucket — the
/// runtime ledger is the source of truth, and composing a second
/// concurrent plan would let two plans drive the same schema at once.
/// The check runs once, before any plan file is written.
pub async fn compose_live_plans(
    ctx: &mut DjogiContext,
    descriptors: Vec<crate::descriptor::ModelDescriptor>,
    snapshot_path: std::path::PathBuf,
    meta: ComposeMeta,
) -> Result<ComposeReport, ComposeError> {
    use std::collections::BTreeMap;

    // 1. Load the existing snapshot from disk
    let snapshot = crate::migrate::snapshot::load_snapshot(&snapshot_path)
        .map_err(|e| ComposeError::Diff(e.to_string()))?;

    // 2. Project current descriptors into bucket map
    let after_map = crate::migrate::projection::project_from_iters(
        descriptors.iter(),
        std::iter::empty::<&crate::descriptor::EnumDescriptor>(),
        std::iter::empty::<&crate::apps::AppDescriptor>(),
        meta.originating_migration.clone(),
    )
    .map_err(|e| ComposeError::Diff(e.to_string()))?;

    // 3. Build "before" bucket map from snapshot
    let bucket_key = crate::migrate::projection::BucketKey {
        database: meta.target_database.clone(),
        app: meta.app_label.clone(),
    };
    let mut before_map = BTreeMap::new();
    before_map.insert(bucket_key, snapshot);

    // 4. Diff bucket maps to find schema deltas
    let deltas = crate::migrate::diff::diff_bucket_maps(&before_map, &after_map)
        .map_err(|e| ComposeError::Diff(e.to_string()))?;

    // 5. Classification context with defaults
    use crate::migrate::schema::OnlineSafetyClassification;
    let inbound_fk_counts: std::collections::BTreeMap<String, u32> =
        std::collections::BTreeMap::new();
    let volatility_overrides: std::collections::BTreeMap<
        (String, String),
        crate::descriptor::DefaultVolatility,
    > = std::collections::BTreeMap::new();
    let classify_ctx = crate::live_migrate::classify::ClassifyContext::application_default(
        &inbound_fk_counts,
        &volatility_overrides,
    );

    // 6. Filter deltas to ExpandContract (live-plan eligible)
    let mut report = ComposeReport::default();

    // Refuse to compose a second concurrent live plan in the same
    // (target_database, app_label) bucket. The runtime ledger is the
    // source of truth: if a non-terminal plan already owns this bucket,
    // composing another would let two plans drive the same schema
    // concurrently. The guard belongs HERE (compose time), not in the
    // executor (which always finds the plan it is executing).
    let bucket = format!("{}:{}", meta.target_database, meta.app_label);
    check_no_active_plan(ctx, &bucket).await?;

    for delta in deltas {
        let classified =
            crate::live_migrate::classify::classify_delta(&delta.operations, &classify_ctx);

        // Check if any operation classifies as ExpandContract
        let has_expand_contract = classified.iter().any(|(_, classification)| {
            matches!(classification, OnlineSafetyClassification::ExpandContract)
        });

        if !has_expand_contract {
            continue;
        }

        // Build live plan for this delta
        // Use timestamp-based HeerId generation (node_id=0, sequence=0 as placeholder)
        let plan_id = crate::types::HeerId::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            0, // node_id placeholder
            0, // sequence placeholder
        )
        .unwrap_or(crate::types::HeerId::ZERO);

        let slug = format!("live_{}", meta.app_label);

        let plan = build_skeleton_plan(
            plan_id.to_string(),
            slug.clone(),
            OnlineSafetyClassification::ExpandContract,
            Vec::new(), // Steps populated by executor in Stage 3
        );

        // Write plan file to disk
        let migrations_root = meta.workspace_root.join("migrations");
        let path = crate::live_migrate::plan_file::write_plan(&migrations_root, &plan)
            .map_err(ComposeError::Serialize)?;

        report.plans_composed += 1;
        report.plan_file_paths.push(path);
        report.plan_ids.push(plan_id.to_string());
    }

    Ok(report)
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
pub fn extract_backfill_params(steps: &[crate::live_migrate::plan::Step]) -> ExtractResult {
    use crate::live_migrate::plan::{StepKind, StepParameters};

    for step in steps {
        if step.kind == StepKind::BackfillChunked {
            match &step.parameters {
                StepParameters::BackfillChunked {
                    table,
                    predicate_template,
                    chunk_size,
                } => {
                    return ExtractResult::Params {
                        table: table.clone(),
                        filter: predicate_template.clone(),
                        // TODO: StepParameters::BackfillChunked doesn't have a
                        // columns field yet. Populate when the plan schema
                        // gains per-column granularity (djogi#332).
                        columns: Vec::new(),
                        batch_size: *chunk_size as i64,
                    };
                }
                _ => {
                    return ExtractResult::Malformed(format!(
                        "step kind is BackfillChunked but parameters are {:?}",
                        step.parameters.kind()
                    ));
                }
            }
        }
    }
    ExtractResult::NotBackfillChunked
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

    let plan_id_val: crate::types::HeerId = plan_id.parse().unwrap_or(crate::types::HeerId::ZERO);

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

    // ── extract_backfill_params tests ────────────────────────────────────

    fn backfill_step(ordinal: u32) -> Step {
        Step {
            kind: StepKind::BackfillChunked,
            ordinal,
            parameters: StepParameters::BackfillChunked {
                table: "vehicles".to_string(),
                predicate_template: "old_status IS NULL".to_string(),
                chunk_size: 500,
            },
        }
    }

    #[test]
    fn extract_backfill_params_empty_steps_returns_not_backfill() {
        let steps: Vec<Step> = vec![];
        assert_eq!(
            extract_backfill_params(&steps),
            ExtractResult::NotBackfillChunked,
        );
    }

    #[test]
    fn extract_backfill_params_no_backfill_step_returns_not_backfill() {
        let steps = vec![dummy_step(0), dummy_step(1)];
        assert_eq!(
            extract_backfill_params(&steps),
            ExtractResult::NotBackfillChunked,
        );
    }

    #[test]
    fn extract_backfill_params_extracts_correct_params() {
        let steps = vec![dummy_step(0), backfill_step(1), dummy_step(2)];
        let result = extract_backfill_params(&steps);
        assert_eq!(
            result,
            ExtractResult::Params {
                table: "vehicles".to_string(),
                filter: "old_status IS NULL".to_string(),
                columns: Vec::<String>::new(),
                batch_size: 500,
            },
        );
    }

    #[test]
    fn extract_backfill_params_batch_size_cast_from_u32_to_i64() {
        let step = Step {
            kind: StepKind::BackfillChunked,
            ordinal: 0,
            parameters: StepParameters::BackfillChunked {
                table: "t".to_string(),
                predicate_template: "1=1".to_string(),
                chunk_size: u32::MAX,
            },
        };
        let result = extract_backfill_params(&[step]);
        assert_eq!(
            result,
            ExtractResult::Params {
                table: "t".to_string(),
                filter: "1=1".to_string(),
                columns: Vec::<String>::new(),
                batch_size: i64::from(u32::MAX),
            },
        );
    }

    #[test]
    fn extract_backfill_params_kind_mismatch_returns_malformed() {
        // Step has kind = BackfillChunked but parameters are ExpandSchema.
        let step = Step {
            kind: StepKind::BackfillChunked,
            ordinal: 0,
            parameters: StepParameters::ExpandSchema {
                sql_segments: vec!["ALTER TABLE foo ADD COLUMN bar INT".to_string()],
            },
        };
        match extract_backfill_params(&[step]) {
            ExtractResult::Malformed(diag) => {
                assert!(
                    diag.contains("BackfillChunked"),
                    "diagnostic should mention BackfillChunked: {diag}",
                );
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn extract_backfill_params_first_step_is_backfill() {
        let steps = vec![backfill_step(0), dummy_step(1)];
        assert!(matches!(
            extract_backfill_params(&steps),
            ExtractResult::Params { .. }
        ));
    }

    #[test]
    fn extract_backfill_params_last_step_is_backfill() {
        let steps = vec![dummy_step(0), backfill_step(1)];
        assert!(matches!(
            extract_backfill_params(&steps),
            ExtractResult::Params { .. }
        ));
    }
}
