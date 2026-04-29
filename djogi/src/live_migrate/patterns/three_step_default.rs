//! Three-step volatile-default add pattern.
//!
//! Covers the "Add column with Postgres-VOLATILE default" row of the
//! v3 plan §7 classification table. The classifier resolves the
//! default expression's volatility via
//! [`crate::migrate::pg_volatility::classify_default_expression`];
//! `Volatility::Volatile` (or unknown identifiers, which default
//! conservatively) routes here. Stable / immutable defaults stay
//! `OnlineSafe` via the Pg18 catalog-only fast path and never reach
//! this pattern.
//!
//! # Operation shape
//!
//! Accepts [`AddColumn`](SchemaOperation::AddColumn) whose
//! [`ColumnSchema::default_sql`](crate::migrate::schema::ColumnSchema::default_sql)
//! is `Some(_)`. The classifier strips off the volatility check
//! before dispatch — the pattern itself does not re-classify the
//! default expression.
//!
//! # Step graph
//!
//! 1. [`StepKind::ExpandSchema`] — `ALTER TABLE <t> ADD COLUMN <c>
//!    <type> NULL` (no `DEFAULT` clause yet — the volatile fragment
//!    cannot land in catalog metadata without rewriting every row).
//! 2. [`StepKind::BeginCompatibilityWindow`] — install a `BEFORE
//!    INSERT` trigger that runs the volatile expression for new
//!    rows. Existing rows still see `NULL` until the backfill
//!    catches up; the dual-write hook flags the column as
//!    transitionally nullable.
//! 3. [`StepKind::BackfillChunked`] — populate the column for
//!    existing rows. The predicate is `WHERE <col> IS NULL`,
//!    structurally idempotent — once a row is backfilled it falls
//!    out of the predicate forever.
//! 4. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//!    until `SELECT count(*) FROM <t> WHERE <col> IS NULL` returns
//!    zero.
//! 5. [`StepKind::FinalizeConstraints`] — `ALTER TABLE <t> ALTER
//!    COLUMN <c> SET DEFAULT <expr>` then `ALTER TABLE <t> ALTER
//!    COLUMN <c> SET NOT NULL`.
//! 6. [`StepKind::CleanupLegacyState`] — `DROP TRIGGER ... ON <t>`
//!    (the `BEFORE INSERT` trigger is no longer needed once the
//!    column carries the default).

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::schema::ColumnSchema;

/// Marker type implementing [`Pattern`] for the three-step volatile
/// default rollout.
pub struct ThreeStepDefault;

impl Pattern for ThreeStepDefault {
    const ID: &'static str = "three_step_default";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let (table, column) = match op {
            SchemaOperation::AddColumn { table, column } => (table, column),
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AddColumn".to_string(),
                });
            }
        };

        let default_expr = column
            .default_sql
            .as_ref()
            .ok_or_else(|| PatternError::CannotEmit {
                pattern: Self::ID,
                reason: "AddColumn carries no DEFAULT — no volatility to stage".to_string(),
            })?;

        let trigger_name = format!("trg_{table}_{}_default", column.name);
        let expand_sql = format!(
            "ALTER TABLE {tbl_q} ADD COLUMN {col_q} {ty} NULL",
            tbl_q = quote_ident(table),
            col_q = quote_ident(&column.name),
            ty = render_sql_type(column),
        );
        let backfill_predicate = format!(
            "WHERE {col_q} IS NULL AND id BETWEEN $1 AND $2",
            col_q = quote_ident(&column.name),
        );
        let gate_query = format!(
            "SELECT count(*) FROM {tbl_q} WHERE {col_q} IS NULL",
            tbl_q = quote_ident(table),
            col_q = quote_ident(&column.name),
        );
        let set_default_sql = format!(
            "ALTER TABLE {tbl_q} ALTER COLUMN {col_q} SET DEFAULT {expr}",
            tbl_q = quote_ident(table),
            col_q = quote_ident(&column.name),
            expr = default_expr,
        );
        let set_not_null_sql = if column.nullable {
            None
        } else {
            Some(format!(
                "ALTER TABLE {tbl_q} ALTER COLUMN {col_q} SET NOT NULL",
                tbl_q = quote_ident(table),
                col_q = quote_ident(&column.name),
            ))
        };
        let drop_trigger_sql = format!(
            "DROP TRIGGER {trg_q} ON {tbl_q}",
            trg_q = quote_ident(&trigger_name),
            tbl_q = quote_ident(table),
        );

        let mut finalize_segments = vec![set_default_sql];
        if let Some(sql) = set_not_null_sql {
            finalize_segments.push(sql);
        }

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
                    hooks: vec![format!(
                        "before_insert_trigger::{table}::{col}::{trg}",
                        col = column.name,
                        trg = trigger_name,
                    )],
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
                kind: StepKind::FinalizeConstraints,
                ordinal: 4,
                parameters: StepParameters::FinalizeConstraints {
                    sql_segments: finalize_segments,
                },
            },
            Step {
                kind: StepKind::CleanupLegacyState,
                ordinal: 5,
                parameters: StepParameters::CleanupLegacyState {
                    sql_segments: vec![drop_trigger_sql],
                },
            },
        ])
    }
}

fn render_sql_type(column: &ColumnSchema) -> String {
    column.sql_type.clone()
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

    fn column(default_sql: Option<&str>, nullable: bool) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: default_sql.map(str::to_string),
            foreign_key: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: "uuid_column".to_string(),
            nullable,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "UUID".to_string(),
            unique: false,
        }
    }

    fn op(default_sql: Option<&str>, nullable: bool) -> SchemaOperation {
        SchemaOperation::AddColumn {
            table: "audit_event".to_string(),
            column: column(default_sql, nullable),
        }
    }

    #[test]
    fn emits_six_step_volatile_default_sequence() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), false), &ctx()).unwrap();
        assert_eq!(steps.len(), 6);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::ExpandSchema,
                StepKind::BeginCompatibilityWindow,
                StepKind::BackfillChunked,
                StepKind::ValidateBackfill,
                StepKind::FinalizeConstraints,
                StepKind::CleanupLegacyState,
            ],
        );
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), false), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn expand_step_omits_default_clause() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), false), &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert!(sql_segments[0].contains("ADD COLUMN"));
        assert!(!sql_segments[0].contains("DEFAULT"));
    }

    #[test]
    fn backfill_predicate_is_idempotent_via_is_null() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), false), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[2].parameters
        else {
            panic!("expected BackfillChunked");
        };
        assert!(predicate_template.contains("IS NULL"));
        assert!(predicate_template.contains("\"uuid_column\""));
    }

    #[test]
    fn finalize_emits_set_default_then_set_not_null() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), false), &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[4].parameters else {
            panic!("expected FinalizeConstraints");
        };
        assert_eq!(sql_segments.len(), 2);
        assert!(sql_segments[0].contains("SET DEFAULT gen_random_uuid()"));
        assert!(sql_segments[1].contains("SET NOT NULL"));
    }

    #[test]
    fn nullable_column_omits_set_not_null() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), true), &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[4].parameters else {
            panic!("expected FinalizeConstraints");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("SET DEFAULT"));
        assert!(!sql_segments.iter().any(|s| s.contains("SET NOT NULL")));
    }

    #[test]
    fn cleanup_drops_before_insert_trigger() {
        let steps = ThreeStepDefault::emit(&op(Some("gen_random_uuid()"), false), &ctx()).unwrap();
        let StepParameters::CleanupLegacyState { sql_segments } = &steps[5].parameters else {
            panic!("expected CleanupLegacyState");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("DROP TRIGGER"));
        assert!(sql_segments[0].contains("\"audit_event\""));
        assert!(sql_segments[0].contains("\"trg_audit_event_uuid_column_default\""));
    }

    #[test]
    fn rejects_add_column_without_default() {
        let err = ThreeStepDefault::emit(&op(None, false), &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::CannotEmit { .. }));
    }

    #[test]
    fn rejects_alter_column() {
        let op = SchemaOperation::AlterColumn {
            table: "audit_event".to_string(),
            column: "uuid_column".to_string(),
            change: crate::migrate::diff::ColumnChange::SetNullable(false),
        };
        let err = ThreeStepDefault::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }
}
