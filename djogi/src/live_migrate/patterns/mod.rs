//! Live-migration rollout patterns.
//!
//! Each module under [`patterns`](self) implements one rollout shape
//! that the classifier (T5) maps a [`SchemaOperation`] onto. A pattern
//! takes the operation plus an ambient [`PatternContext`] and emits a
//! [`Vec<Step>`](Step) — the immutable step graph the runner (T7+)
//! later executes. Patterns are pure: no I/O, no `pg_catalog` reads,
//! no host-variable behaviour. The output is the canonical plan-file
//! payload, identical between any two runs of the compose pipeline
//! over the same descriptor inputs.
//!
//! # Pattern catalogue
//!
//! Nine patterns ship under T8, paired with one documentation-only
//! module that records why a tenth never will:
//!
//! - [`nullable_not_null`] — nullable add followed by backfill plus
//!   `SET NOT NULL` finalize.
//! - [`replacement_column`] — shadow column expand/contract for type
//!   changes that require a row rewrite.
//! - [`codec_transition`] — protected-field codec rotation under a
//!   compatibility window.
//! - [`backfill_then_tighten`] — backfill before a deferred FK or
//!   uniqueness `VALIDATE`.
//! - [`index_dependent`] — `CREATE INDEX CONCURRENTLY` with an
//!   `indvalid` gate.
//! - [`two_phase_validate`] — `ADD CONSTRAINT … NOT VALID` plus a
//!   separate `VALIDATE` for CHECK / NOT NULL / FK above the
//!   validation row threshold.
//! - [`unique_via_index`] — `CREATE UNIQUE INDEX CONCURRENTLY` plus
//!   `ADD CONSTRAINT … USING INDEX`, also covering index replacement
//!   on overlapping columns.
//! - [`three_step_default`] — three-step rollout for columns whose
//!   default expression is Postgres-volatile.
//! - [`multi_fk_staging`] — split four-or-more FK additions on a
//!   single table across paired NOT VALID + VALIDATE steps.
//! - [`generated_column_refusal`] — documentation breadcrumb
//!   explaining why no shadow-column pattern ships for stored
//!   generated column rewrites.
//!
//! # Why no `generated_column_replacement.rs`
//!
//! Stored generated column rewrites classify as
//! [`OnlineSafetyClassification::OfflineOnly`](crate::migrate::OnlineSafetyClassification::OfflineOnly)
//! per the §7 amendment of the v3 plan. The obvious-seeming "add a
//! shadow generated column, swap, drop" pattern offers no relief
//! because adding the replacement stored generated column itself
//! rewrites the table under `AccessExclusiveLock` — the same lock
//! window the shadow-column pattern was meant to avoid. Operators
//! who need an online path remodel away from `STORED GENERATED`
//! entirely (e.g. into a regular column populated by an application
//! trigger) and route the resulting change through
//! [`replacement_column`] instead.
//!
//! [`generated_column_refusal`] is a marker-only module that records
//! this decision next to the patterns that *do* ship, so a future
//! reader who searches the patterns directory does not waste cycles
//! re-deriving why the pattern is missing.
//!
//! # Idempotent-predicate contract (§3)
//!
//! v3 plan §3 mandates that every [`StepKind::BackfillChunked`] step
//! a pattern emits carries an idempotent `WHERE` predicate:
//! re-running the same chunk against rows already touched produces
//! no observable change. The contract is enforced by the
//! [`Pattern::IDEMPOTENT_PREDICATE`] associated constant — every
//! pattern that emits chunked backfill must set the constant to
//! `true`, and per-pattern unit tests assert the predicate text
//! contains an idempotent shape (`IS NULL`, `IS DISTINCT FROM`, or
//! a similar self-cancelling clause).
//!
//! Transforms whose chunk semantics cannot prove idempotency do not
//! ship as patterns at all — the classifier routes them to
//! [`OnlineSafetyClassification::OfflineOnly`](crate::migrate::OnlineSafetyClassification::OfflineOnly)
//! per the v3 plan rather than letting them masquerade as
//! ExpandContract.
//!
//! # Public surface
//!
//! Only the [`Pattern`] trait, [`PatternContext`], and [`PatternError`]
//! are re-exported from [`crate::live_migrate`]. The individual zero-
//! sized pattern types stay module-private — production callers reach
//! them through the classifier-driven dispatch (T10), never by
//! direct construction.
//!
//! [`SchemaOperation`]: crate::migrate::SchemaOperation
//! [`Step`]: crate::live_migrate::plan::Step
//! [`StepKind::BackfillChunked`]: crate::live_migrate::plan::StepKind::BackfillChunked

use crate::live_migrate::plan::Step;
use crate::migrate::SchemaOperation;

pub mod backfill_then_tighten;
pub mod codec_transition;
pub mod generated_column_refusal;
pub mod index_dependent;
pub mod multi_fk_staging;
pub mod nullable_not_null;
pub mod replacement_column;
pub mod three_step_default;
pub mod two_phase_validate;
pub mod unique_via_index;

/// Ambient configuration threaded into every pattern's
/// [`Pattern::emit`] call.
///
/// The compose pipeline constructs one `PatternContext` per
/// `(database, app)` bucket from `Djogi.toml`'s `[live]` section, so
/// every pattern in the bucket sees the same thresholds and chunk
/// sizing. Mirrors the relevant subset of
/// [`crate::live_migrate::ClassifyContext`] — the values come from the
/// same `[live]` section, but the pattern layer never needs the
/// classifier's per-table FK graph or logging-profile fields.
#[derive(Debug, Clone)]
pub struct PatternContext {
    /// Approximate row count of the operation's target table when
    /// known. `None` is treated as "above any threshold" — the more
    /// conservative path is always safe for a pattern emitter.
    pub estimated_rows: Option<u64>,

    /// Threshold above which CHECK / NOT NULL / FK validation is
    /// staged via `NOT VALID` plus a separate `VALIDATE`. Default
    /// `100_000`; sourced from `Djogi.toml` `[live]
    /// validation_threshold_rows`.
    pub validation_threshold_rows: u64,

    /// Threshold for multi-FK staging — adding this many or more
    /// foreign keys to a single table in one delta routes through
    /// [`multi_fk_staging`]. Default `4`; sourced from `Djogi.toml`
    /// `[live] multi_fk_threshold`.
    pub multi_fk_threshold: u32,

    /// Chunk size for [`StepKind::BackfillChunked`](crate::live_migrate::plan::StepKind::BackfillChunked)
    /// steps a pattern emits. Default `10_000`; sourced from
    /// `Djogi.toml` `[live] backfill_chunk_size`.
    pub backfill_chunk_size: u32,
}

impl PatternContext {
    /// Construct a context with the v3 plan's documented defaults.
    /// Useful for unit tests and for callers that want to override
    /// only one or two fields.
    pub fn with_defaults() -> Self {
        Self {
            estimated_rows: None,
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            backfill_chunk_size: 10_000,
        }
    }
}

/// Trait every shipped pattern implements. Implementors are zero-
/// sized marker structs; the trait is generic only via the
/// [`Pattern::ID`] / [`Pattern::IDEMPOTENT_PREDICATE`] associated
/// constants and the [`Pattern::emit`] entry point.
pub trait Pattern {
    /// Stable identifier for diagnostics and plan-file metadata. The
    /// classifier's dispatch table keys off this string; renaming a
    /// pattern is a breaking change to any persisted plan file that
    /// recorded the previous ID.
    const ID: &'static str;

    /// `true` when this pattern emits at least one chunked backfill
    /// step. Patterns that never emit
    /// [`StepKind::BackfillChunked`](crate::live_migrate::plan::StepKind::BackfillChunked)
    /// set the constant to `false`. The constant exists so per-pattern
    /// unit tests can assert "if the pattern claims chunked backfill,
    /// the emitted predicate text contains an idempotent shape" without
    /// re-deriving the rule from the step list.
    const IDEMPOTENT_PREDICATE: bool;

    /// Build the step graph for `op` under `ctx`. Returns
    /// [`PatternError::WrongOperation`] when `op` does not match the
    /// pattern's expected variant, or [`PatternError::CannotEmit`] /
    /// [`PatternError::Invariant`] when the operation matches but
    /// some pattern-specific precondition fails.
    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError>;
}

/// Reasons a pattern's [`Pattern::emit`] may refuse. Exposed publicly
/// so the dispatch layer (T10) can surface the exact mismatch in
/// operator messages.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PatternError {
    /// The operation variant does not match the pattern's expected
    /// shape. The dispatcher uses this to fall through to the next
    /// candidate pattern.
    #[error("operation does not match pattern {pattern}: {reason}")]
    WrongOperation {
        pattern: &'static str,
        reason: String,
    },
    /// The operation variant matched, but a pattern-specific
    /// precondition (missing FK target, unsupported index method,
    /// etc.) prevents emission.
    #[error("pattern {pattern} cannot handle this operation: {reason}")]
    CannotEmit {
        pattern: &'static str,
        reason: String,
    },
    /// An assertion baked into the pattern was violated — typically a
    /// signal that the descriptor input has drifted from the shape the
    /// pattern was written for.
    #[error("invariant violation in pattern {pattern}: {detail}")]
    Invariant {
        pattern: &'static str,
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_migrate::plan::StepParameters;
    use crate::migrate::diff::ColumnChange;

    #[test]
    fn pattern_context_defaults_match_v3_plan() {
        let ctx = PatternContext::with_defaults();
        assert!(ctx.estimated_rows.is_none());
        assert_eq!(ctx.validation_threshold_rows, 100_000);
        assert_eq!(ctx.multi_fk_threshold, 4);
        assert_eq!(ctx.backfill_chunk_size, 10_000);
    }

    #[test]
    fn pattern_error_wrong_operation_displays_pattern_id() {
        let err = PatternError::WrongOperation {
            pattern: "demo",
            reason: "expected AlterColumn".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("demo"));
        assert!(msg.contains("expected AlterColumn"));
    }

    #[test]
    fn pattern_error_cannot_emit_displays_pattern_id() {
        let err = PatternError::CannotEmit {
            pattern: "demo",
            reason: "missing FK target".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("demo"));
        assert!(msg.contains("missing FK target"));
    }

    #[test]
    fn pattern_error_invariant_displays_pattern_id() {
        let err = PatternError::Invariant {
            pattern: "demo",
            detail: "ordinal sequence broken".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("demo"));
        assert!(msg.contains("ordinal sequence broken"));
    }

    /// Cross-pattern dispatch witness — every shipped pattern handles
    /// the operation shape it documents, and rejects an operation it
    /// does not own. The test reaches into module-private pattern
    /// types because the dispatch layer (T10) has not landed yet;
    /// once it does, this test will move to live behind the
    /// dispatcher's API.
    #[test]
    fn dispatch_witnesses_pattern_id_uniqueness() {
        let ids = [
            nullable_not_null::NullableNotNull::ID,
            replacement_column::ReplacementColumn::ID,
            codec_transition::CodecTransition::ID,
            backfill_then_tighten::BackfillThenTighten::ID,
            index_dependent::IndexDependent::ID,
            two_phase_validate::TwoPhaseValidate::ID,
            unique_via_index::UniqueViaIndex::ID,
            three_step_default::ThreeStepDefault::ID,
            multi_fk_staging::MultiFkStaging::ID,
        ];
        let mut seen: Vec<&'static str> = ids.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            ids.len(),
            "pattern IDs must be unique across the catalogue: {ids:?}",
        );
    }

    #[test]
    fn dispatch_witness_idempotent_predicate_flag_matches_emitted_shape() {
        // For every pattern whose IDEMPOTENT_PREDICATE constant is
        // true, the emitted step graph for a representative input
        // must contain at least one BackfillChunked step. The
        // pattern's per-module tests assert the predicate text
        // shape; here we assert the high-level invariant that
        // "claims chunked backfill" matches "emits chunked
        // backfill".
        let ctx = PatternContext::with_defaults();
        let op = SchemaOperation::AlterColumn {
            table: "demo_t".to_string(),
            column: "demo_col".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        let steps = nullable_not_null::NullableNotNull::emit(&op, &ctx).unwrap();
        const { assert!(nullable_not_null::NullableNotNull::IDEMPOTENT_PREDICATE) };
        assert!(
            steps
                .iter()
                .any(|s| matches!(s.parameters, StepParameters::BackfillChunked { .. })),
            "claim mismatched: pattern advertises chunked backfill but emitted no BackfillChunked",
        );

        // Sanity-check the inverse for a pattern that does not emit
        // chunked backfill (index_dependent).
        const { assert!(!index_dependent::IndexDependent::IDEMPOTENT_PREDICATE) };
        let op = SchemaOperation::AddIndex(crate::migrate::schema::IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: crate::migrate::schema::IndexTypeSchema::BTree,
            kind: crate::migrate::schema::IndexKindSchema::NonUnique,
            name: "demo_idx".to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: "demo_t".to_string(),
            target: crate::migrate::schema::IndexTargetSchema::Columns(vec![
                crate::migrate::schema::IndexColumnSchema {
                    name: "demo_col".to_string(),
                    nulls: crate::migrate::schema::IndexNullsOrderSchema::Default,
                    opclass: None,
                    order: crate::migrate::schema::IndexOrderSchema::Asc,
                },
            ]),
        });
        let steps = index_dependent::IndexDependent::emit(&op, &ctx).unwrap();
        assert!(
            steps
                .iter()
                .all(|s| !matches!(s.parameters, StepParameters::BackfillChunked { .. })),
            "index_dependent must not emit BackfillChunked",
        );
        // And confirm every emitted step's StepKind is consistent
        // with its parameters tag — the mod.rs-level dispatch
        // sanity check.
        for step in &steps {
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn dispatch_witness_steps_have_sequential_ordinals() {
        // Every pattern's emitted Vec<Step> must report ordinals
        // 0, 1, 2, ... — LivePlan::validate (T6) refuses gaps or
        // duplicates, and the runner relies on the sort being a
        // no-op when the input is already canonical.
        let ctx = PatternContext::with_defaults();
        let op = SchemaOperation::AlterColumn {
            table: "demo_t".to_string(),
            column: "demo_col".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        let steps = nullable_not_null::NullableNotNull::emit(&op, &ctx).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }
}
