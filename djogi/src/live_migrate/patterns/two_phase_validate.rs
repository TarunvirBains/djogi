//! Two-phase constraint validation pattern.
//! Covers the "Add CHECK constraint to table > validation threshold",
//! "Add NOT NULL constraint to populated table" (via the
//! `CHECK (col IS NOT NULL)` shim documented in §7), and "Add FK
//! constraint to table > validation threshold" rows of the v3 plan
//! §7 classification table. Exclusion constraints route to
//! [`OnlineSafetyClassification::OfflineOnly`](crate::migrate::OnlineSafetyClassification::OfflineOnly)
//! per §7 — Postgres 18 does not accept `NOT VALID` for `EXCLUDE`,
//! so two-phase staging is structurally impossible.
//! # Operation shape
//! Accepts [`AddForeignKey`](SchemaOperation::AddForeignKey) — the
//! only constraint kind currently exposed on
//! [`SchemaOperation`](crate::migrate::SchemaOperation) for which the
//! validation-threshold escalation applies. CHECK / NOT NULL routing
//! lands when those operations gain dedicated variants.
//! # Step graph
//! 1. [`StepKind::ExpandSchema`] — `ALTER TABLE <t> ADD CONSTRAINT
//! <name> FOREIGN KEY (<col>) REFERENCES <ref_t>(<ref_col>) NOT
//! VALID`. The `NOT VALID` suffix tells Postgres to skip the
//! validation pass that would otherwise scan every row under an
//! `AccessExclusiveLock`.
//! 2. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//! while `ALTER TABLE <t> VALIDATE CONSTRAINT <name>` runs. The
//! `VALIDATE` pass takes a `ShareUpdateExclusiveLock` on the
//! target table — readers and writers continue while the scan
//! completes.
//! No backfill is needed — `VALIDATE` reads existing rows in place;
//! it never rewrites them. [`Pattern::IDEMPOTENT_PREDICATE`] is
//! `false` because no [`StepKind::BackfillChunked`] step is emitted.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::schema::OnDeleteSchema;

/// Marker type implementing [`Pattern`] for the
/// `NOT VALID` + `VALIDATE` constraint sequence.
pub struct TwoPhaseValidate;

impl Pattern for TwoPhaseValidate {
    const ID: &'static str = "two_phase_validate";
    const IDEMPOTENT_PREDICATE: bool = false;

    fn emit(op: &SchemaOperation, _ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
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
        let on_delete = render_on_delete(fk.on_delete);
        let mut expand_sql = format!(
            "ALTER TABLE {tbl_q} ADD CONSTRAINT {name_q} FOREIGN KEY ({col_q}) REFERENCES {ref_tbl_q}({ref_col_q}) ON DELETE {on_delete}",
            tbl_q = quote_ident(table),
            name_q = quote_ident(&constraint_name),
            col_q = quote_ident(column),
            ref_tbl_q = quote_ident(&fk.ref_table),
            ref_col_q = quote_ident(&fk.ref_column),
        );
        if fk.deferrable {
            expand_sql.push_str(" DEFERRABLE");
            if fk.initially_deferred {
                expand_sql.push_str(" INITIALLY DEFERRED");
            }
        }
        expand_sql.push_str(" NOT VALID");

        let validate_sql = format!(
            "ALTER TABLE {tbl_q} VALIDATE CONSTRAINT {name_q}",
            tbl_q = quote_ident(table),
            name_q = quote_ident(&constraint_name),
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
                kind: StepKind::ValidateBackfill,
                ordinal: 1,
                parameters: StepParameters::ValidateBackfill {
                    gate_query: validate_sql,
                },
            },
        ])
    }
}

fn render_on_delete(disc: OnDeleteSchema) -> &'static str {
    match disc {
        OnDeleteSchema::Restrict => "RESTRICT",
        OnDeleteSchema::Cascade => "CASCADE",
        OnDeleteSchema::SetNull => "SET NULL",
        OnDeleteSchema::SetDefault => "SET DEFAULT",
        OnDeleteSchema::NoAction => "NO ACTION",
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
    use crate::migrate::schema::ForeignKeySchema;

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn op_with_cascade(cascade: OnDeleteSchema) -> SchemaOperation {
        SchemaOperation::AddForeignKey {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            fk: ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: cascade,
                ref_column: "id".to_string(),
                ref_table: "owner".to_string(),
            },
        }
    }

    #[test]
    fn emits_two_step_validate_sequence() {
        let steps =
            TwoPhaseValidate::emit(&op_with_cascade(OnDeleteSchema::Cascade), &ctx()).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::ValidateBackfill);
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let steps =
            TwoPhaseValidate::emit(&op_with_cascade(OnDeleteSchema::Restrict), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn expand_step_emits_not_valid_with_cascade() {
        let steps =
            TwoPhaseValidate::emit(&op_with_cascade(OnDeleteSchema::Cascade), &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("NOT VALID"));
        assert!(sql_segments[0].contains("ON DELETE CASCADE"));
        assert!(sql_segments[0].contains("\"vehicle\""));
        assert!(sql_segments[0].contains("\"owner\""));
        assert!(sql_segments[0].contains("\"fk_vehicle_owner_id\""));
    }

    #[test]
    fn validate_step_emits_validate_constraint() {
        let steps =
            TwoPhaseValidate::emit(&op_with_cascade(OnDeleteSchema::Restrict), &ctx()).unwrap();
        let StepParameters::ValidateBackfill { gate_query } = &steps[1].parameters else {
            panic!("expected ValidateBackfill");
        };
        assert!(gate_query.contains("VALIDATE CONSTRAINT"));
        assert!(gate_query.contains("\"fk_vehicle_owner_id\""));
    }

    #[test]
    fn deferrable_fk_emits_deferrable_initially_deferred_clause() {
        let op = SchemaOperation::AddForeignKey {
            table: "vehicle".to_string(),
            column: "owner_id".to_string(),
            fk: ForeignKeySchema {
                deferrable: true,
                initially_deferred: true,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "owner".to_string(),
            },
        };
        let steps = TwoPhaseValidate::emit(&op, &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert!(sql_segments[0].contains("DEFERRABLE"));
        assert!(sql_segments[0].contains("INITIALLY DEFERRED"));
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
        let err = TwoPhaseValidate::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }
}
