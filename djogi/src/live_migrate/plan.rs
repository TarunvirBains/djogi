//! Live migration plan structures — the typed step graph that `live`
//! plans materialise into JSON on disk and into rows in
//! [`crate::live_migrate::state::LivePlanRow`] in Postgres.
//!
//! # Plan-file vs runtime-state separation (§1 D2)
//!
//! Two artefacts back every live migration:
//!
//! 1. **The plan file** — an immutable JSON document on disk that
//!    encodes the *definition* of the rollout: which steps run, in
//!    which order, with which parameters. Generated once by T8's
//!    pattern emitters, never edited after `djogi live run` opens it.
//! 2. **The DB row** — a mutable row in `djogi_live_plans` that tracks
//!    the *runtime state*: where the operator is in the step graph,
//!    backfill progress, last error, completion timestamp. Updated by
//!    every checkpoint write.
//!
//! The split is the load-bearing safety invariant of Phase 7.5: a plan
//! cannot diverge from its own definition. The DB row records the SHA-256
//! of the file at first run; checksum mismatch on resume aborts the run
//! with an actionable refusal ("plan file edited after start;
//! re-generate or abandon and retry").
//!
//! # Step graph shape
//!
//! The step graph is a flat ordered list, not a DAG. Each step depends
//! solely on the previous one having completed. Operator gates
//! ([`StepKind::ValidateBackfill`], [`StepKind::CutoverReads`],
//! [`StepKind::FinalizeConstraints`]) split sequential execution from
//! operator-driven phases — the runner pauses and surfaces the gate;
//! the operator drives the next-step transition explicitly via the CLI
//! (T10).
//!
//! # Stability
//!
//! [`StepKind`], [`PlanClassification`], and [`StepParameters`] are
//! [`#[non_exhaustive]`] so future patterns / classifications can land
//! without a breaking change. Downstream `match` against these enums
//! from outside the `djogi` crate must include a wildcard arm.

use serde::{Deserialize, Serialize};

use crate::migrate::OnlineSafetyClassification;
use crate::types::HeerId;

// ── StepKind ──────────────────────────────────────────────────────────

/// One step in a live migration plan. Each variant maps to one row in
/// the on-disk plan file's `steps` array; the runner dispatches each
/// step to the matching executor in T7+.
///
/// `#[non_exhaustive]` because the v3 plan §3 explicitly anticipates
/// new step kinds landing in later phases (e.g. a future
/// `RebuildIndexConcurrently` pattern). Adding a variant must not break
/// downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepKind {
    /// Emit additive DDL (new column, new index, etc.). The first step
    /// in every expand → backfill → flip → contract sequence.
    ExpandSchema,
    /// Operator hook that opens the dual-read / dual-write
    /// compatibility window. Runtime services consult the active
    /// plan's compat hooks before every read / write during the window.
    BeginCompatibilityWindow,
    /// Chunked backfill execution. The chunk predicate must be
    /// idempotent — see §3 line 420 of the v3 plan.
    BackfillChunked,
    /// Operator gate. The runner refuses to advance past this step
    /// until a SELECT-style gate query returns the expected count /
    /// shape (e.g. zero rows still carrying the legacy projection).
    ValidateBackfill,
    /// Flip the read path. Visage projections, query filters, and
    /// admin reads switch to the new schema.
    CutoverReads,
    /// Flip the write path. New writes go to the contracted schema
    /// only; the compat window's dual-write is dropped.
    CutoverWrites,
    /// Add the deferred constraints (NOT NULL, FK, unique) now that
    /// data is correct. Runs after both read- and write-cutovers.
    FinalizeConstraints,
    /// Drop the legacy column / index / table that the expand step
    /// added a parallel for. The terminal step in the standard
    /// expand → contract sequence.
    CleanupLegacyState,
    /// v3 fallback for plans that compose Phase 7 reversible operations
    /// without wrapping them in an expand/contract shape. Used by
    /// patterns whose work is one Phase 7 op surrounded by gates rather
    /// than a full expand → backfill → flip → contract sequence.
    RunReversibleSchemaOp,
}

// ── PlanClassification ────────────────────────────────────────────────

/// The classification recorded in the `djogi_live_plans.classification`
/// CHECK constraint. A subset of [`OnlineSafetyClassification`]:
/// `FastLockDestructiveGuarded` is **not** a live-plan classification
/// (the operator pulls the trigger directly via `--allow-destructive`),
/// so the CHECK constraint omits it.
///
/// `#[non_exhaustive]` — future classifications that route through a
/// live plan can land without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanClassification {
    /// Pure additive change. Recorded for plans the runner could have
    /// applied directly but where the operator chose to drive the
    /// rollout through `live` for staging / dry-run reasons.
    OnlineSafe,
    /// The dominant classification. Cannot complete safely in a single
    /// segment — Phase 7.5 generates a live plan and the operator
    /// drives the expand → backfill → flip → contract sequence.
    ExpandContract,
    /// Djogi refuses to emit SQL automatically. Operator is performing
    /// the change by hand; the plan file documents the intended
    /// sequence for audit / runbook purposes.
    OfflineOnly,
}

impl From<OnlineSafetyClassification> for Option<PlanClassification> {
    /// Lossless mapping from the four-variant online-safety verdict
    /// down to the three-variant plan-file classification. The lone
    /// dropped variant — `FastLockDestructiveGuarded` — does not
    /// route through Phase 7.5, so `None` is the correct
    /// "no plan emitted; operator runs Phase 7 with
    /// `--allow-destructive`" signal.
    fn from(value: OnlineSafetyClassification) -> Self {
        match value {
            OnlineSafetyClassification::OnlineSafe => Some(PlanClassification::OnlineSafe),
            OnlineSafetyClassification::ExpandContract => Some(PlanClassification::ExpandContract),
            OnlineSafetyClassification::OfflineOnly => Some(PlanClassification::OfflineOnly),
            OnlineSafetyClassification::FastLockDestructiveGuarded => None,
        }
    }
}

impl PlanClassification {
    /// String form used in the `djogi_live_plans.classification` CHECK
    /// constraint. Keep in lockstep with the SQL CHECK clause in
    /// [`crate::live_migrate::state::INSTALL_SQL`].
    pub const fn as_db_str(self) -> &'static str {
        match self {
            PlanClassification::OnlineSafe => "online_safe",
            PlanClassification::ExpandContract => "expand_contract",
            PlanClassification::OfflineOnly => "offline_only",
        }
    }

    /// Inverse of [`PlanClassification::as_db_str`]. Returns `None`
    /// for strings not in the CHECK list — callers surface this as a
    /// database-corruption indicator.
    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "online_safe" => PlanClassification::OnlineSafe,
            "expand_contract" => PlanClassification::ExpandContract,
            "offline_only" => PlanClassification::OfflineOnly,
            _ => return None,
        })
    }
}

// ── StepParameters ────────────────────────────────────────────────────

/// Per-step parameters. Each variant carries the smallest payload the
/// matching executor needs; variant tags align with [`StepKind`] so the
/// runner can pair `Step.kind` with `Step.parameters` by enum tag.
///
/// `#[non_exhaustive]` because T8's pattern emitters will refine which
/// fields each variant carries. The serde tag is the kind name in
/// `snake_case`; all internal field names follow the same convention so
/// JSON consumers (T11 visage admin) can decode without per-variant
/// per-field rename annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepParameters {
    /// Additive DDL fragments to execute in order. Each fragment is
    /// expected to be a single Postgres statement; the runner does not
    /// split on `;`.
    ExpandSchema { sql_segments: Vec<String> },
    /// Hook IDs to register with the runtime when the compatibility
    /// window opens. Hook resolution happens in T9; this step records
    /// the IDs only.
    BeginCompatibilityWindow { hooks: Vec<String> },
    /// Chunked backfill description. `predicate_template` must be an
    /// idempotent SQL expression — re-running the same chunk against
    /// already-backfilled rows must produce no observable change.
    BackfillChunked {
        table: String,
        predicate_template: String,
        chunk_size: u32,
    },
    /// Operator gate query — the runner pauses on this step and
    /// surfaces the query result; the operator decides whether to
    /// advance.
    ValidateBackfill { gate_query: String },
    /// Free-text description shown to the operator at the read-cutover
    /// gate. The actual flip happens in application config / runtime
    /// hooks; the step records the operator-facing prompt.
    CutoverReads { description: String },
    /// Free-text description shown to the operator at the write-cutover
    /// gate. Same shape as `CutoverReads`.
    CutoverWrites { description: String },
    /// DDL fragments that finalise constraints (NOT NULL, FK,
    /// unique) once data is correct.
    FinalizeConstraints { sql_segments: Vec<String> },
    /// DDL fragments that drop the legacy column / index / table.
    CleanupLegacyState { sql_segments: Vec<String> },
    /// v3 fallback — a Phase 7 reversible op packaged with its
    /// matching `down` so the live runner can dispatch it the same
    /// way Phase 7's runner does.
    RunReversibleSchemaOp { up_sql: String, down_sql: String },
}

impl StepParameters {
    /// Returns the [`StepKind`] tag matching this variant. Used by
    /// [`Step::is_consistent`] to detect on-disk plans whose
    /// `kind` field has drifted from the `parameters.kind` discriminator.
    pub const fn kind(&self) -> StepKind {
        match self {
            StepParameters::ExpandSchema { .. } => StepKind::ExpandSchema,
            StepParameters::BeginCompatibilityWindow { .. } => StepKind::BeginCompatibilityWindow,
            StepParameters::BackfillChunked { .. } => StepKind::BackfillChunked,
            StepParameters::ValidateBackfill { .. } => StepKind::ValidateBackfill,
            StepParameters::CutoverReads { .. } => StepKind::CutoverReads,
            StepParameters::CutoverWrites { .. } => StepKind::CutoverWrites,
            StepParameters::FinalizeConstraints { .. } => StepKind::FinalizeConstraints,
            StepParameters::CleanupLegacyState { .. } => StepKind::CleanupLegacyState,
            StepParameters::RunReversibleSchemaOp { .. } => StepKind::RunReversibleSchemaOp,
        }
    }
}

// ── Step ──────────────────────────────────────────────────────────────

/// One entry in a live plan's step list. The `ordinal` is the step's
/// position in the sequence (0-based) and is recorded explicitly so the
/// JSON document stays self-describing under partial reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The step's discriminator. Doubles as the ledger
    /// `current_step` value when the runner advances onto this step.
    pub kind: StepKind,
    /// Position in the step sequence (0-based). Steps are sorted on
    /// `ordinal` after deserialization; gaps and duplicates are
    /// rejected by [`LivePlan::validate`].
    pub ordinal: u32,
    /// Per-kind parameters. The variant tag MUST match `kind` —
    /// see [`Step::is_consistent`].
    pub parameters: StepParameters,
}

impl Step {
    /// Returns `true` iff [`Step::kind`] matches the variant of
    /// [`Step::parameters`]. Used by [`LivePlan::validate`] to reject
    /// hand-edited plan files whose `kind` field drifted from the
    /// nested `parameters.kind` discriminator.
    pub fn is_consistent(&self) -> bool {
        self.kind == self.parameters.kind()
    }
}

// ── PlanHeader ────────────────────────────────────────────────────────

/// Top-level metadata recorded once per plan file. The header is
/// emitted alongside the step list so a plan can be identified without
/// inspecting any individual step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHeader {
    /// HeerId — surfaced as the `djogi_live_plans.plan_id` primary
    /// key. Serialised as a JSON string (HeerId's canonical form) so
    /// JS callers preserve precision.
    pub plan_id: HeerId,
    /// Operator-facing slug, included in the on-disk filename. Same
    /// shape as a migration version's slug body — ASCII letters,
    /// digits, and underscores only (validated by the compose pipeline
    /// before this struct is constructed).
    pub slug: String,
    /// Subset of [`OnlineSafetyClassification`] — the v3 plan §3
    /// CHECK constraint accepts three values.
    pub classification: PlanClassification,
    /// Phase 7 migration version that triggered this plan. Empty
    /// string when the plan was generated outside the runner (rare —
    /// e.g. an operator-authored runbook plan).
    pub originating_migration: String,
    /// Which of the three Djogi databases (`main`, `crud_log`,
    /// `event_log`) this plan targets. Defaults to `main`.
    pub target_database: String,
    /// App label this plan belongs to. Empty string for the synthetic
    /// global bucket.
    pub app_label: String,
}

// ── LivePlan ──────────────────────────────────────────────────────────

/// A complete live plan — header plus ordered step list. This is the
/// shape serialised to disk under
/// `migrations/<target>/live/<plan_id>_<slug>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePlan {
    pub header: PlanHeader,
    pub steps: Vec<Step>,
}

impl LivePlan {
    /// Validate the in-memory plan. Returns [`PlanValidationError`] on
    /// any of:
    ///
    /// - any step's `kind` disagrees with its `parameters` variant tag,
    /// - the step list is empty,
    /// - ordinals don't form `0..steps.len()` exactly (gap or duplicate),
    /// - the slug contains a byte that is not ASCII-alphanumeric or
    ///   underscore (the on-disk filename derives directly from the
    ///   slug, so non-portable bytes would corrupt the path).
    ///
    /// Called by [`crate::live_migrate::plan_file::write_plan`] before
    /// the file is written and by
    /// [`crate::live_migrate::plan_file::read_plan`] after parsing.
    pub fn validate(&self) -> Result<(), PlanValidationError> {
        if self.steps.is_empty() {
            return Err(PlanValidationError::EmptySteps);
        }
        for (idx, step) in self.steps.iter().enumerate() {
            if !step.is_consistent() {
                return Err(PlanValidationError::KindMismatch {
                    ordinal: step.ordinal,
                    declared: step.kind,
                    parameters_kind: step.parameters.kind(),
                });
            }
            let expected = u32::try_from(idx).map_err(|_| PlanValidationError::TooManySteps)?;
            if step.ordinal != expected {
                return Err(PlanValidationError::OrdinalGap {
                    position: idx,
                    expected,
                    observed: step.ordinal,
                });
            }
        }
        validate_slug_bytes(&self.header.slug)?;
        Ok(())
    }
}

/// Reasons [`LivePlan::validate`] can refuse a plan. Exposed publicly
/// so the runner can surface the precise mismatch in operator messages
/// without re-deriving the structural rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanValidationError {
    /// The plan carries zero steps — a plan with no work is not a
    /// plan, and the runner refuses to register it.
    #[error("live plan has no steps")]
    EmptySteps,
    /// More than `u32::MAX` steps. Practically impossible; surfaced
    /// for completeness so the `try_from(usize)` arm is total.
    #[error("live plan has more than u32::MAX steps")]
    TooManySteps,
    /// A step's declared `kind` disagreed with its `parameters`
    /// variant tag. Typically signals a hand-edited plan file.
    #[error(
        "step ordinal {ordinal}: kind {declared:?} disagrees with parameters tag {parameters_kind:?}"
    )]
    KindMismatch {
        ordinal: u32,
        declared: StepKind,
        parameters_kind: StepKind,
    },
    /// The step list's `ordinal` field skipped a number or duplicated
    /// one — ordinals must form `0..steps.len()` exactly.
    #[error("step at position {position}: expected ordinal {expected}, observed {observed}")]
    OrdinalGap {
        position: usize,
        expected: u32,
        observed: u32,
    },
    /// The header's slug contained a byte outside the portable
    /// `[A-Za-z0-9_]` (described in plain English: ASCII letter, ASCII
    /// digit, or underscore) set used for the on-disk filename. The
    /// offset and offending byte are reported so the operator can
    /// pinpoint the bad character.
    #[error("slug contains non-portable byte 0x{byte:02x} at offset {offset}")]
    SlugByte { offset: usize, byte: u8 },
    /// The header's slug was the empty string.
    #[error("slug is empty")]
    EmptySlug,
}

/// Validate that every byte of `slug` is an ASCII letter, ASCII digit,
/// or underscore. Per the no-regex policy the rule is byte-level; per
/// CLAUDE.md the rule is spelled out in plain English in the doc
/// comment rather than as a bracket-class shorthand.
fn validate_slug_bytes(slug: &str) -> Result<(), PlanValidationError> {
    if slug.is_empty() {
        return Err(PlanValidationError::EmptySlug);
    }
    for (offset, &byte) in slug.as_bytes().iter().enumerate() {
        let portable = byte.is_ascii_alphanumeric() || byte == b'_';
        if !portable {
            return Err(PlanValidationError::SlugByte { offset, byte });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_step(kind: StepKind, ordinal: u32) -> Step {
        let parameters = match kind {
            StepKind::ExpandSchema => StepParameters::ExpandSchema {
                sql_segments: vec!["ALTER TABLE foo ADD COLUMN bar INT".to_string()],
            },
            StepKind::BeginCompatibilityWindow => StepParameters::BeginCompatibilityWindow {
                hooks: vec!["dual_read_foo_bar".to_string()],
            },
            StepKind::BackfillChunked => StepParameters::BackfillChunked {
                table: "foo".to_string(),
                predicate_template: "WHERE bar IS NULL AND id BETWEEN $1 AND $2".to_string(),
                chunk_size: 10_000,
            },
            StepKind::ValidateBackfill => StepParameters::ValidateBackfill {
                gate_query: "SELECT COUNT(*) FROM foo WHERE bar IS NULL".to_string(),
            },
            StepKind::CutoverReads => StepParameters::CutoverReads {
                description: "flip read path to use bar".to_string(),
            },
            StepKind::CutoverWrites => StepParameters::CutoverWrites {
                description: "flip write path to use bar".to_string(),
            },
            StepKind::FinalizeConstraints => StepParameters::FinalizeConstraints {
                sql_segments: vec!["ALTER TABLE foo ALTER COLUMN bar SET NOT NULL".to_string()],
            },
            StepKind::CleanupLegacyState => StepParameters::CleanupLegacyState {
                sql_segments: vec!["ALTER TABLE foo DROP COLUMN baz".to_string()],
            },
            StepKind::RunReversibleSchemaOp => StepParameters::RunReversibleSchemaOp {
                up_sql: "CREATE INDEX idx_foo ON foo(bar)".to_string(),
                down_sql: "DROP INDEX idx_foo".to_string(),
            },
        };
        Step {
            kind,
            ordinal,
            parameters,
        }
    }

    fn sample_plan() -> LivePlan {
        LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "demo_slug".to_string(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428010203__demo".to_string(),
                target_database: "main".to_string(),
                app_label: "".to_string(),
            },
            steps: vec![
                sample_step(StepKind::ExpandSchema, 0),
                sample_step(StepKind::BackfillChunked, 1),
                sample_step(StepKind::CutoverReads, 2),
            ],
        }
    }

    #[test]
    fn step_kind_round_trips_through_serde_json() {
        // Exhaustive — adding a StepKind variant trips this.
        let all = [
            StepKind::ExpandSchema,
            StepKind::BeginCompatibilityWindow,
            StepKind::BackfillChunked,
            StepKind::ValidateBackfill,
            StepKind::CutoverReads,
            StepKind::CutoverWrites,
            StepKind::FinalizeConstraints,
            StepKind::CleanupLegacyState,
            StepKind::RunReversibleSchemaOp,
        ];
        for kind in all {
            let s = serde_json::to_string(&kind).unwrap();
            let back: StepKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn step_kind_serializes_in_snake_case() {
        let s = serde_json::to_string(&StepKind::BeginCompatibilityWindow).unwrap();
        assert_eq!(s, "\"begin_compatibility_window\"");
        let s = serde_json::to_string(&StepKind::RunReversibleSchemaOp).unwrap();
        assert_eq!(s, "\"run_reversible_schema_op\"");
    }

    #[test]
    fn plan_classification_round_trips_through_serde_json() {
        for c in [
            PlanClassification::OnlineSafe,
            PlanClassification::ExpandContract,
            PlanClassification::OfflineOnly,
        ] {
            let s = serde_json::to_string(&c).unwrap();
            let back: PlanClassification = serde_json::from_str(&s).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn plan_classification_db_str_round_trip() {
        for c in [
            PlanClassification::OnlineSafe,
            PlanClassification::ExpandContract,
            PlanClassification::OfflineOnly,
        ] {
            assert_eq!(PlanClassification::from_db_str(c.as_db_str()), Some(c));
        }
        assert_eq!(PlanClassification::from_db_str("nope"), None);
    }

    #[test]
    fn online_safety_classification_to_plan_classification() {
        assert_eq!(
            <Option<PlanClassification>>::from(OnlineSafetyClassification::OnlineSafe),
            Some(PlanClassification::OnlineSafe)
        );
        assert_eq!(
            <Option<PlanClassification>>::from(OnlineSafetyClassification::ExpandContract),
            Some(PlanClassification::ExpandContract)
        );
        assert_eq!(
            <Option<PlanClassification>>::from(OnlineSafetyClassification::OfflineOnly),
            Some(PlanClassification::OfflineOnly)
        );
        // The load-bearing case — fast-lock destructive does NOT get
        // a live plan; the operator drives Phase 7 directly with
        // `--allow-destructive`.
        assert_eq!(
            <Option<PlanClassification>>::from(
                OnlineSafetyClassification::FastLockDestructiveGuarded
            ),
            None
        );
    }

    #[test]
    fn step_parameters_kind_returns_matching_tag() {
        for kind in [
            StepKind::ExpandSchema,
            StepKind::BeginCompatibilityWindow,
            StepKind::BackfillChunked,
            StepKind::ValidateBackfill,
            StepKind::CutoverReads,
            StepKind::CutoverWrites,
            StepKind::FinalizeConstraints,
            StepKind::CleanupLegacyState,
            StepKind::RunReversibleSchemaOp,
        ] {
            let step = sample_step(kind, 0);
            assert_eq!(step.parameters.kind(), kind);
            assert!(step.is_consistent());
        }
    }

    #[test]
    fn step_inconsistent_when_kind_disagrees_with_parameters() {
        let mut step = sample_step(StepKind::ExpandSchema, 0);
        step.kind = StepKind::CleanupLegacyState;
        assert!(!step.is_consistent());
    }

    #[test]
    fn live_plan_round_trips_through_serde_json() {
        let plan = sample_plan();
        let s = serde_json::to_string_pretty(&plan).unwrap();
        let back: LivePlan = serde_json::from_str(&s).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn live_plan_plan_id_serializes_as_string() {
        // HeerId's serialize_display impl emits a string. The runner
        // depends on this so JS callers don't lose precision on the
        // 64-bit id.
        let plan = sample_plan();
        let s = serde_json::to_string(&plan).unwrap();
        assert!(
            s.contains("\"plan_id\":\"0\""),
            "plan_id must serialize as a JSON string; got: {s}",
        );
    }

    #[test]
    fn live_plan_validate_accepts_well_formed_plan() {
        let plan = sample_plan();
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn live_plan_validate_rejects_kind_mismatch() {
        let mut plan = sample_plan();
        plan.steps[0].kind = StepKind::CleanupLegacyState;
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::KindMismatch {
                ordinal: 0,
                declared: StepKind::CleanupLegacyState,
                parameters_kind: StepKind::ExpandSchema,
            }
        ));
    }

    #[test]
    fn live_plan_validate_rejects_ordinal_gap() {
        let mut plan = sample_plan();
        plan.steps[1].ordinal = 5;
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::OrdinalGap {
                position: 1,
                expected: 1,
                observed: 5
            }
        ));
    }

    #[test]
    fn live_plan_validate_rejects_empty_steps() {
        let mut plan = sample_plan();
        plan.steps.clear();
        assert!(matches!(
            plan.validate().unwrap_err(),
            PlanValidationError::EmptySteps
        ));
    }

    #[test]
    fn live_plan_validate_rejects_non_portable_slug_byte() {
        let mut plan = sample_plan();
        plan.header.slug = "bad slug".to_string();
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::SlugByte {
                offset: 3,
                byte: b' '
            }
        ));
    }

    #[test]
    fn live_plan_validate_rejects_empty_slug() {
        let mut plan = sample_plan();
        plan.header.slug = "".to_string();
        assert!(matches!(
            plan.validate().unwrap_err(),
            PlanValidationError::EmptySlug
        ));
    }

    #[test]
    fn step_ordinal_sort_is_stable() {
        let mut steps = [
            sample_step(StepKind::CutoverReads, 2),
            sample_step(StepKind::ExpandSchema, 0),
            sample_step(StepKind::BackfillChunked, 1),
        ];
        steps.sort_by_key(|s| s.ordinal);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::BackfillChunked);
        assert_eq!(steps[2].kind, StepKind::CutoverReads);
    }
}
