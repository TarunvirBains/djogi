//! Backfill-before-tightening pattern.
//! Covers the "Add FK constraint to populated table > validation
//! threshold" and "Add composite unique" rows of the v3 plan §7
//! classification table when existing rows must first be scrubbed to
//! satisfy the constraint. The segment that adds the
//! constraint emitted it `NOT VALID` (or added the column with the FK
//! left dangling); this pattern handles the data-cleaning step graph
//! before the operator-driven `VALIDATE`.
//! # Operation shape
//! Accepts [`AddForeignKey`](SchemaOperation::AddForeignKey) — the
//! delta carries the FK's target table / column so the gate query
//! and finalize SQL can reference both endpoints.
//! # Step graph
//! 1. [`StepKind::ExpandSchema`] — sentinel record. already
//!    added the column / `NOT VALID` constraint; this pattern owns
//!    only the validation half.
//! 2. [`StepKind::BackfillChunked`] — UPDATE rows whose `<col>`
//!    fails the FK predicate. The pattern emits a complete
//!    UPDATE-tail fragment of the shape
//!    ```sql
//!    SET <col> = NULL
//!    WHERE id IN (SELECT id FROM <table>
//!                 WHERE <col> IS NOT NULL
//!                   AND <col> NOT IN (SELECT <ref_col> FROM <ref_table>)
//!                 LIMIT $1)
//!    ```
//! Idempotent — once the offending rows have been nulled (or
//! remediated by the operator out-of-band), they fall out of the
//! inner predicate forever. `LIMIT $1` bounds the row count to one
//! chunk; `$1` is the only placeholder the runner binds.
//! 3. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//! until the count of violators is zero.
//! 4. [`StepKind::FinalizeConstraints`] — `ALTER TABLE <t> VALIDATE
//! CONSTRAINT <name>`.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;

/// Marker type implementing [`Pattern`] for the
/// scrub-then-validate sequence.
pub struct BackfillThenTighten;

impl Pattern for BackfillThenTighten {
    const ID: &'static str = "backfill_then_tighten";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let (table, column, fk) = match op {
            SchemaOperation::AddForeignKey { table, column, fk } => (table, column, fk),
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AddForeignKey".to_string(),
                });
            }
        };

        let constraint_name = format!("fk_{table}_{column}");
        // Backfill template: `SET <col> = NULL` for rows whose FK
        // value points at a missing parent. The conservative null-out
        // fix avoids guessing a "right" parent — the operator can
        // re-row the affected records out-of-band before the
        // VALIDATE CONSTRAINT step lands. The inner predicate
        // self-cancels: once a row is nulled, it falls out of
        // `<col> IS NOT NULL` forever. LIMIT $1 is the only
        // placeholder the runner binds.
        let backfill_predicate = format!(
            "SET {col_q} = NULL WHERE id IN (SELECT id FROM {tbl_q} WHERE {col_q} IS NOT NULL AND {col_q} NOT IN (SELECT {ref_col_q} FROM {ref_tbl_q}) LIMIT $1)",
            col_q = quote_ident(column),
            tbl_q = quote_ident(table),
            ref_col_q = quote_ident(&fk.ref_column),
            ref_tbl_q = quote_ident(&fk.ref_table),
        );
        let gate_query = format!(
            "SELECT count(*) FROM {tbl_q} WHERE {col_q} IS NOT NULL AND {col_q} NOT IN (SELECT {ref_col_q} FROM {ref_tbl_q})",
            tbl_q = quote_ident(table),
            col_q = quote_ident(column),
            ref_col_q = quote_ident(&fk.ref_column),
            ref_tbl_q = quote_ident(&fk.ref_table),
        );
        let finalize_sql = format!(
            "ALTER TABLE {tbl_q} VALIDATE CONSTRAINT {name_q}",
            tbl_q = quote_ident(table),
            name_q = quote_ident(&constraint_name),
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
    use crate::migrate::schema::{ForeignKeySchema, OnDeleteSchema};

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn op() -> SchemaOperation {
        SchemaOperation::AddForeignKey {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            fk: ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "owner".to_string(),
            },
        }
    }

    #[test]
    fn emits_canonical_four_step_sequence() {
        let steps = BackfillThenTighten::emit(&op(), &ctx()).unwrap();
        assert_eq!(steps.len(), 4);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::ExpandSchema,
                StepKind::BackfillChunked,
                StepKind::ValidateBackfill,
                StepKind::FinalizeConstraints,
            ],
        );
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let steps = BackfillThenTighten::emit(&op(), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn backfill_template_emits_complete_update_tail_with_set_and_limit() {
        let steps = BackfillThenTighten::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[1].parameters
        else {
            panic!("expected BackfillChunked");
        };
        assert!(
            predicate_template.contains("SET \"owner_id\" = NULL"),
            "template must null-out offending FK column: {predicate_template}",
        );
        assert!(predicate_template.contains("\"owner\""));
        assert!(predicate_template.contains("\"id\""));
        assert!(
            predicate_template.contains("NOT IN") && predicate_template.contains("IS NOT NULL"),
            "predicate must isolate violators idempotently: {predicate_template}",
        );
        assert!(
            predicate_template.contains("LIMIT $1"),
            "template must bound row count via LIMIT $1: {predicate_template}",
        );
        assert!(
            predicate_template.contains("WHERE id IN (SELECT id FROM"),
            "template must use canonical id-IN-subquery shape: {predicate_template}",
        );
        assert!(
            !predicate_template.contains("$2"),
            "template must not bind any placeholder beyond $1: {predicate_template}",
        );
    }

    #[test]
    fn backfill_template_concatenates_into_valid_update_statement() {
        let steps = BackfillThenTighten::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            table,
            predicate_template,
            ..
        } = &steps[1].parameters
        else {
            panic!("expected BackfillChunked");
        };
        let stmt = format!("UPDATE {table} {predicate_template}");
        assert!(stmt.contains("UPDATE"));
        assert!(stmt.contains("SET"));
        assert!(stmt.contains("WHERE"));
        assert!(stmt.contains("LIMIT $1"));
        let set_pos = stmt.find("SET").unwrap();
        let where_pos = stmt.find("WHERE").unwrap();
        assert!(set_pos < where_pos, "SET must precede WHERE: {stmt}");
    }

    #[test]
    fn finalize_emits_validate_constraint() {
        let steps = BackfillThenTighten::emit(&op(), &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[3].parameters else {
            panic!("expected FinalizeConstraints");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("VALIDATE CONSTRAINT"));
        assert!(sql_segments[0].contains("\"vehicle\""));
        assert!(sql_segments[0].contains("\"fk_vehicle_owner_id\""));
    }

    #[test]
    fn rejects_drop_foreign_key() {
        let op = SchemaOperation::DropForeignKey {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            fk: ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "owner".to_string(),
            },
        };
        let err = BackfillThenTighten::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn rejects_unrelated_alter_column() {
        let op = SchemaOperation::AlterColumn {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            change: crate::migrate::diff::ColumnChange::SetNullable(false),
        };
        let err = BackfillThenTighten::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }
}
