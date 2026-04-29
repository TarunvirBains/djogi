//! Nullable add → backfill → `SET NOT NULL` finalize pattern.
//!
//! Covers the "Add NOT NULL constraint to populated table" row of the
//! v3 plan §7 classification table. The descriptor change that
//! triggers this pattern is an
//! [`AlterColumn`](SchemaOperation::AlterColumn) carrying
//! [`ColumnChange::SetNullable(false)`] — i.e. an existing nullable
//! column being tightened to `NOT NULL`. The column itself was added
//! as nullable in an earlier Phase 7 segment; this pattern handles
//! the finalization step graph that makes the constraint flip safe
//! against live writers.
//!
//! # Step graph
//!
//! 1. [`StepKind::ExpandSchema`] — sentinel record. The actual
//!    `ADD COLUMN` happened in Phase 7 (or in a previous live plan
//!    via [`replacement_column`](super::replacement_column)); this
//!    pattern records a no-op expand fragment so the plan-file shape
//!    matches the canonical expand → backfill → finalize sequence
//!    other patterns share.
//! 2. [`StepKind::BackfillChunked`] — populate rows where the column
//!    is currently `NULL`. The predicate is `WHERE <col> IS NULL`,
//!    which is structurally idempotent — once a row is no longer
//!    `NULL`, subsequent chunk re-runs skip it.
//! 3. [`StepKind::ValidateBackfill`] — operator gate. The runner
//!    pauses until `SELECT count(*) FROM <table> WHERE <col> IS
//!    NULL` returns zero.
//! 4. [`StepKind::FinalizeConstraints`] — `ALTER TABLE <table> ALTER
//!    COLUMN <col> SET NOT NULL`.
//!
//! # Idempotency
//!
//! The chunked-backfill predicate `IS NULL` is the canonical
//! self-cancelling shape — once the chunk's UPDATE writes a non-null
//! value, the row falls out of the predicate forever. Re-running a
//! chunk against a partially-completed range is a no-op.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::diff::ColumnChange;

/// Marker type implementing [`Pattern`] for the nullable-to-not-null
/// finalize sequence. Zero-sized — no per-instance state, the
/// pattern is fully described by the operation it is asked to emit
/// against.
pub struct NullableNotNull;

impl Pattern for NullableNotNull {
    const ID: &'static str = "nullable_not_null";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
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

        let backfill_predicate = format!(
            "WHERE {col} IS NULL AND id BETWEEN $1 AND $2",
            col = quote_ident(column),
        );
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
                kind: StepKind::BackfillChunked,
                ordinal: 1,
                parameters: StepParameters::BackfillChunked {
                    table: table.clone(),
                    predicate_template: backfill_predicate,
                    chunk_size: ctx.backfill_chunk_size,
                },
            },
            Step {
                kind: StepKind::ValidateBackfill,
                ordinal: 2,
                parameters: StepParameters::ValidateBackfill { gate_query },
            },
            Step {
                kind: StepKind::FinalizeConstraints,
                ordinal: 3,
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
    fn emits_canonical_four_step_sequence() {
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::BackfillChunked);
        assert_eq!(steps[2].kind, StepKind::ValidateBackfill);
        assert_eq!(steps[3].kind, StepKind::FinalizeConstraints);
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
    fn backfill_predicate_uses_idempotent_is_null_shape() {
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[1].parameters
        else {
            panic!("expected BackfillChunked at ordinal 1");
        };
        assert!(
            predicate_template.contains("IS NULL"),
            "predicate must be self-cancelling: {predicate_template}",
        );
        assert!(
            predicate_template.contains("\"owner_id\""),
            "predicate must reference the target column: {predicate_template}",
        );
    }

    #[test]
    fn finalize_emits_set_not_null() {
        let steps = NullableNotNull::emit(&op(), &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[3].parameters else {
            panic!("expected FinalizeConstraints at ordinal 3");
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
