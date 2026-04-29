//! Replacement-column expand/contract pattern.
//!
//! Covers the "Change column type — requires rewrite" row of the v3
//! plan §7 classification table. The descriptor change that triggers
//! this pattern is an [`AlterColumn`](SchemaOperation::AlterColumn)
//! carrying [`ColumnChange::ChangeType`] whose `from`/`to` rendered
//! types differ in storage shape (varchar widening within `Pg18`
//! catalog-only paths classifies as `OnlineSafe` and never reaches
//! this pattern).
//!
//! # Step graph
//!
//! 1. [`StepKind::ExpandSchema`] — `ALTER TABLE <t> ADD COLUMN
//!    <c>_new <to> NULL`. Adds the shadow column the rewrite drains
//!    into.
//! 2. [`StepKind::BeginCompatibilityWindow`] — register the dual-
//!    read / dual-write hooks that keep `<c>` and `<c>_new` aligned
//!    across in-flight transactions.
//! 3. [`StepKind::BackfillChunked`] — copy `<c>` into `<c>_new`. The
//!    predicate is `WHERE <c>_new IS DISTINCT FROM <c>` so re-runs
//!    skip rows already converged.
//! 4. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//!    until `SELECT count(*) FROM <t> WHERE <c>_new IS DISTINCT FROM
//!    <c>` returns zero.
//! 5. [`StepKind::CutoverReads`] — visage projection switches reads
//!    to `<c>_new`.
//! 6. [`StepKind::CutoverWrites`] — writes target `<c>_new` only.
//!    Dual-write hook turns off.
//! 7. [`StepKind::FinalizeConstraints`] — apply NOT NULL / CHECK on
//!    `<c>_new` if the original column carried them.
//! 8. [`StepKind::CleanupLegacyState`] — `DROP COLUMN <c>` then
//!    `RENAME COLUMN <c>_new TO <c>`.
//!
//! # Idempotency
//!
//! `IS DISTINCT FROM` is the canonical row-equality predicate that
//! handles `NULL` symmetrically — once a row converges, subsequent
//! chunk runs see `<c>_new = <c>` and skip the row.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::diff::ColumnChange;

/// Marker type implementing [`Pattern`] for the shadow-column type
/// change rollout.
pub struct ReplacementColumn;

impl Pattern for ReplacementColumn {
    const ID: &'static str = "replacement_column";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let (table, column, to_type) = match op {
            SchemaOperation::AlterColumn {
                table,
                column,
                change: ColumnChange::ChangeType { to, .. },
            } => (table, column, to),
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AlterColumn { change: ChangeType }".to_string(),
                });
            }
        };

        let shadow = format!("{column}_new");
        let expand_sql = format!(
            "ALTER TABLE {tbl} ADD COLUMN {shadow_q} {ty} NULL",
            tbl = quote_ident(table),
            shadow_q = quote_ident(&shadow),
            ty = to_type,
        );
        let backfill_predicate = format!(
            "WHERE {shadow_q} IS DISTINCT FROM {col_q} AND id BETWEEN $1 AND $2",
            shadow_q = quote_ident(&shadow),
            col_q = quote_ident(column),
        );
        let gate_query = format!(
            "SELECT count(*) FROM {tbl} WHERE {shadow_q} IS DISTINCT FROM {col_q}",
            tbl = quote_ident(table),
            shadow_q = quote_ident(&shadow),
            col_q = quote_ident(column),
        );
        let drop_legacy_sql = format!(
            "ALTER TABLE {tbl} DROP COLUMN {col_q}",
            tbl = quote_ident(table),
            col_q = quote_ident(column),
        );
        let rename_sql = format!(
            "ALTER TABLE {tbl} RENAME COLUMN {shadow_q} TO {col_q}",
            tbl = quote_ident(table),
            shadow_q = quote_ident(&shadow),
            col_q = quote_ident(column),
        );

        Ok(vec![
            Step {
                kind: StepKind::ExpandSchema,
                ordinal: 0,
                parameters: StepParameters::ExpandSchema {
                    sql_segments: vec![expand_sql],
                },
            },
            Step {
                kind: StepKind::BeginCompatibilityWindow,
                ordinal: 1,
                parameters: StepParameters::BeginCompatibilityWindow {
                    hooks: vec![
                        format!("dual_read::{table}::{column}"),
                        format!("dual_write::{table}::{column}"),
                    ],
                },
            },
            Step {
                kind: StepKind::BackfillChunked,
                ordinal: 2,
                parameters: StepParameters::BackfillChunked {
                    table: table.clone(),
                    predicate_template: backfill_predicate,
                    chunk_size: ctx.backfill_chunk_size,
                },
            },
            Step {
                kind: StepKind::ValidateBackfill,
                ordinal: 3,
                parameters: StepParameters::ValidateBackfill { gate_query },
            },
            Step {
                kind: StepKind::CutoverReads,
                ordinal: 4,
                parameters: StepParameters::CutoverReads {
                    description: format!("flip read path for {table}.{column} onto {shadow}"),
                },
            },
            Step {
                kind: StepKind::CutoverWrites,
                ordinal: 5,
                parameters: StepParameters::CutoverWrites {
                    description: format!(
                        "flip write path for {table}.{column} onto {shadow}; drop dual-write",
                    ),
                },
            },
            Step {
                kind: StepKind::FinalizeConstraints,
                ordinal: 6,
                parameters: StepParameters::FinalizeConstraints {
                    sql_segments: Vec::new(),
                },
            },
            Step {
                kind: StepKind::CleanupLegacyState,
                ordinal: 7,
                parameters: StepParameters::CleanupLegacyState {
                    sql_segments: vec![drop_legacy_sql, rename_sql],
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

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn op() -> SchemaOperation {
        SchemaOperation::AlterColumn {
            table: "ledger_entry".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "INTEGER".to_string(),
                to: "BIGINT".to_string(),
            },
        }
    }

    #[test]
    fn emits_eight_step_expand_contract_sequence() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        assert_eq!(steps.len(), 8);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::ExpandSchema,
                StepKind::BeginCompatibilityWindow,
                StepKind::BackfillChunked,
                StepKind::ValidateBackfill,
                StepKind::CutoverReads,
                StepKind::CutoverWrites,
                StepKind::FinalizeConstraints,
                StepKind::CleanupLegacyState,
            ],
        );
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn expand_step_adds_shadow_column_with_target_type() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("ADD COLUMN"));
        assert!(sql_segments[0].contains("\"amount_new\""));
        assert!(sql_segments[0].contains("BIGINT"));
        assert!(sql_segments[0].contains("NULL"));
    }

    #[test]
    fn backfill_predicate_is_idempotent_via_is_distinct_from() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[2].parameters
        else {
            panic!("expected BackfillChunked");
        };
        assert!(predicate_template.contains("IS DISTINCT FROM"));
        assert!(predicate_template.contains("\"amount_new\""));
        assert!(predicate_template.contains("\"amount\""));
    }

    #[test]
    fn cleanup_drops_legacy_then_renames_shadow() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        let StepParameters::CleanupLegacyState { sql_segments } = &steps[7].parameters else {
            panic!("expected CleanupLegacyState");
        };
        assert_eq!(sql_segments.len(), 2);
        assert!(sql_segments[0].contains("DROP COLUMN \"amount\""));
        assert!(sql_segments[1].contains("RENAME COLUMN \"amount_new\" TO \"amount\""));
    }

    #[test]
    fn rejects_non_change_type_alter_column() {
        let op = SchemaOperation::AlterColumn {
            table: "ledger_entry".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        let err = ReplacementColumn::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn rejects_drop_column() {
        let op = SchemaOperation::DropColumn {
            table: "ledger_entry".to_string(),
            column: "amount".to_string(),
        };
        let err = ReplacementColumn::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }
}
