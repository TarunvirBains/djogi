//! Nullable add → backfill → `SET NOT NULL` finalize pattern.
//! Covers the "Add NOT NULL constraint to populated table" row of the
//! v3 plan §7 classification table. The descriptor change that
//! triggers this pattern is an
//! [`AlterColumn`](SchemaOperation::AlterColumn) carrying
//! [`ColumnChange::SetNullable(false)`] — i.e. an existing nullable
//! column being tightened to `NOT NULL`. The column itself was added
//! as nullable in an earlier segment; this pattern handles
//! the finalization step graph that makes the constraint flip safe
//! against live writers.
//! # Step graph
//! 1. [`StepKind::ExpandSchema`] — sentinel record. The actual
//! `ADD COLUMN` happened in (or in a previous live plan
//! via [`replacement_column`](super::replacement_column)); this
//! pattern records a no-op expand fragment so the plan-file shape
//! matches the canonical expand → validate → finalize sequence
//! other patterns share.
//! 2. [`StepKind::ValidateBackfill`] — operator gate. The runner
//! pauses until `SELECT count(*) FROM <table> WHERE <col> IS
//! NULL` returns zero. Filling existing NULL rows is the
//! operator's responsibility — the
//! [`SchemaOperation::AlterColumn`] delta carrying
//! [`ColumnChange::SetNullable(false)`] does NOT itself supply a
//! backfill expression, so this pattern intentionally omits a
//! [`StepKind::BackfillChunked`] step. The operator either backfills
//! the column out-of-band (e.g. via an application-side migration
//! that writes the missing values) or routes the change through
//! [`super::three_step_default`] / [`super::replacement_column`]
//! when an expression is available.
//! 3. [`StepKind::FinalizeConstraints`] — `ALTER TABLE <table> ALTER
//! COLUMN <col> SET NOT NULL`.
//! # Idempotency
//! The validate gate is naturally idempotent: re-checking the
//! `WHERE <col> IS NULL` count is read-only. The pattern emits no
//! chunked backfill, so the §3 idempotent-predicate contract is
//! satisfied vacuously; [`Pattern::IDEMPOTENT_PREDICATE`] is left
//! `true` to mean "any backfill predicate this pattern would emit
//! would be idempotent", which the absence of a chunked step trivially
//! satisfies.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::diff::ColumnChange;

// Suppress the unused-context warning: the trait signature requires the
// ambient context, and other patterns consume `ctx.backfill_chunk_size`
// when they emit chunked backfill. This pattern emits no chunked
// backfill (see module-level docs) but must still accept the parameter.

/// Marker type implementing [`Pattern`] for the nullable-to-not-null
/// finalize sequence. Zero-sized — no per-instance state, the
/// pattern is fully described by the operation it is asked to emit
/// against.
pub struct NullableNotNull;

impl Pattern for NullableNotNull {
    const ID: &'static str = "nullable_not_null";
    /// `false` because this pattern never emits a chunked backfill
    /// the §3 idempotent-predicate contract is therefore satisfied
    /// vacuously. See the module-level docstring for why no chunked
    /// step ships.
    const IDEMPOTENT_PREDICATE: bool = false;

    fn emit(op: &SchemaOperation, _ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let (table, column) = match op {
            SchemaOperation::AlterColumn {
                table,
                column,
                change: ColumnChange::SetNullable(false),
            } => (table, column),
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AlterColumn { change: SetNullable(false) }".to_string(),
                });
            }
        };

        let gate_query = format!(
            "SELECT count(*) FROM {tbl} WHERE {col} IS NULL",
            tbl = quote_ident(table),
            col = quote_ident(column),
        );
        let finalize_sql = format!(
            "ALTER TABLE {tbl} ALTER COLUMN {col} SET NOT NULL",
            tbl = quote_ident(table),
            col = quote_ident(column),
        );

        Ok(vec![
            Step {
                kind: StepKind::ExpandSchema,
                ordinal: 0,
                parameters: StepParameters::ExpandSchema {
                    sql_segments: Vec::new(),
                },
            },
            Step {
                kind: StepKind::ValidateBackfill,
                ordinal: 1,
                parameters: StepParameters::ValidateBackfill { gate_query },
            },
            Step {
                kind: StepKind::FinalizeConstraints,
                ordinal: 2,
                parameters: StepParameters::FinalizeConstraints {
                    sql_segments: vec![finalize_sql],
                },
            },
        ])
    }
}

/// Wrap a Postgres identifier in double quotes. Identifiers that
/// already contain a double-quote byte are rejected by the higher
/// layers (the descriptor's name validator); this helper assumes its
/// input is a plain identifier and only handles the wrapping.
fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    out.push_str(name);
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn op() -> SchemaOperation {
        SchemaOperation::AlterColumn {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            change: ColumnChange::SetNullable(false),
        }
    }

    #[test]
    fn emits_canonical_three_step_sequence() {
        // The pattern intentionally omits BackfillChunked — see the
        // module-level docstring. Filling existing NULL rows is the
        // operator's responsibility; the validate gate refuses to
        // proceed until the count is zero.
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::ValidateBackfill);
        assert_eq!(steps[2].kind, StepKind::FinalizeConstraints);
    }

    #[test]
    fn emits_no_backfill_chunked_step() {
        // Defensive: NullableNotNull must not emit any BackfillChunked
        // step. Filling NULLs is operator responsibility because the
        // delta does not carry a backfill expression.
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        assert!(
            steps
                .iter()
                .all(|s| !matches!(s.parameters, StepParameters::BackfillChunked { .. })),
            "nullable_not_null must NOT emit BackfillChunked",
        );
    }

    #[test]
    fn emitted_step_ordinals_are_sequential() {
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn validate_gate_uses_is_null_count() {
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        let StepParameters::ValidateBackfill { gate_query } = &steps[1].parameters else {
            panic!("expected ValidateBackfill at ordinal 1");
        };
        assert!(gate_query.contains("count(*)"));
        assert!(gate_query.contains("IS NULL"));
        assert!(gate_query.contains("\"owner_id\""));
        assert!(gate_query.contains("\"vehicle\""));
    }

    #[test]
    fn finalize_emits_set_not_null() {
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[2].parameters else {
            panic!("expected FinalizeConstraints at ordinal 2");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("SET NOT NULL"));
        assert!(sql_segments[0].contains("\"vehicle\""));
        assert!(sql_segments[0].contains("\"owner_id\""));
    }

    #[test]
    fn rejects_set_nullable_true() {
        let op = SchemaOperation::AlterColumn {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            change: ColumnChange::SetNullable(true),
        };
        let err = NullableNotNull::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn rejects_unrelated_operation_kind() {
        let op = SchemaOperation::DropColumn {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
        };
        let err = NullableNotNull::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }
}
