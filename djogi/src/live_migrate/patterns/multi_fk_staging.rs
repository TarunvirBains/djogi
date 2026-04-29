//! Multi-FK staging pattern.
//!
//! Covers the "Add 4+ FKs to a single table in one bucket diff" row
//! of the v3 plan §7 classification table. The classifier's per-table
//! FK addition counter escalates the bucket entry to ExpandContract;
//! this pattern lays out the staged plan so each FK runs as a
//! `NOT VALID` add followed by a separate `VALIDATE` step. The
//! `VALIDATE` runs under `ShareUpdateExclusiveLock`, so interleaving
//! the steps keeps the lock window per-FK rather than across the
//! whole bucket.
//!
//! # Operation shape
//!
//! Accepts [`AddTable`](SchemaOperation::AddTable). The pattern walks
//! [`TableSchema::columns`](crate::migrate::schema::TableSchema::columns)
//! for entries with `foreign_key.is_some()` and emits two steps per
//! FK: an `ADD CONSTRAINT … NOT VALID` expand step and a `VALIDATE
//! CONSTRAINT` validate step. Tables with fewer FKs than
//! [`PatternContext::multi_fk_threshold`] are routed elsewhere by
//! the classifier and refused here as
//! [`PatternError::CannotEmit`].
//!
//! # Step graph (4 FKs = 8 steps)
//!
//! - Ordinal 0 : `ALTER TABLE … ADD CONSTRAINT fk1 … NOT VALID`
//! - Ordinal 1 : `ALTER TABLE … VALIDATE CONSTRAINT fk1`
//! - Ordinal 2 : `ALTER TABLE … ADD CONSTRAINT fk2 … NOT VALID`
//! - Ordinal 3 : `ALTER TABLE … VALIDATE CONSTRAINT fk2`
//! - …
//!
//! No backfill is emitted — the FK addition validates existing rows
//! in place. [`Pattern::IDEMPOTENT_PREDICATE`] is `false`.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::schema::{ColumnSchema, OnDeleteSchema};

/// Marker type implementing [`Pattern`] for the staged multi-FK
/// validate sequence.
pub struct MultiFkStaging;

impl Pattern for MultiFkStaging {
    const ID: &'static str = "multi_fk_staging";
    const IDEMPOTENT_PREDICATE: bool = false;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let table = match op {
            SchemaOperation::AddTable(table) => table,
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AddTable carrying FK columns".to_string(),
                });
            }
        };

        let fk_columns: Vec<&ColumnSchema> = table
            .columns
            .iter()
            .filter(|c| c.foreign_key.is_some())
            .collect();
        let threshold = usize::try_from(ctx.multi_fk_threshold).unwrap_or(usize::MAX);
        if fk_columns.len() < threshold {
            return Err(PatternError::CannotEmit {
                pattern: Self::ID,
                reason: format!(
                    "table has {} FKs; multi_fk_threshold is {}",
                    fk_columns.len(),
                    ctx.multi_fk_threshold,
                ),
            });
        }

        let mut steps = Vec::with_capacity(fk_columns.len() * 2);
        let mut ordinal: u32 = 0;
        for col in &fk_columns {
            // Safe: we filtered on `foreign_key.is_some()` above, so the
            // unwrap-via-match cannot reach the None arm.
            let fk = match col.foreign_key.as_ref() {
                Some(fk) => fk,
                None => {
                    return Err(PatternError::Invariant {
                        pattern: Self::ID,
                        detail: format!(
                            "column {:?} lost its FK between filter and emit",
                            col.name
                        ),
                    });
                }
            };
            let constraint_name = format!("fk_{}_{}", table.table, col.name);
            let mut expand_sql = format!(
                "ALTER TABLE {tbl_q} ADD CONSTRAINT {name_q} FOREIGN KEY ({col_q}) REFERENCES {ref_tbl_q}({ref_col_q}) ON DELETE {on_delete}",
                tbl_q = quote_ident(&table.table),
                name_q = quote_ident(&constraint_name),
                col_q = quote_ident(&col.name),
                ref_tbl_q = quote_ident(&fk.ref_table),
                ref_col_q = quote_ident(&fk.ref_column),
                on_delete = render_on_delete(fk.on_delete),
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
                tbl_q = quote_ident(&table.table),
                name_q = quote_ident(&constraint_name),
            );

            steps.push(Step {
                kind: StepKind::ExpandSchema,
                ordinal,
                parameters: StepParameters::ExpandSchema {
                    sql_segments: vec![expand_sql],
                },
            });
            ordinal = ordinal.saturating_add(1);
            steps.push(Step {
                kind: StepKind::ValidateBackfill,
                ordinal,
                parameters: StepParameters::ValidateBackfill {
                    gate_query: validate_sql,
                },
            });
            ordinal = ordinal.saturating_add(1);
        }
        Ok(steps)
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
    use crate::migrate::schema::{
        ForeignKeySchema, PkKindSchema, PrimaryKeySchema, RelationKindSchema, TableSchema,
    };

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn fk_column(name: &str, ref_table: &str) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: Some(ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: ref_table.to_string(),
            }),
            index_type: None,
            indexed: false,
            max_length: None,
            name: name.to_string(),
            nullable: false,
            on_delete: Some(OnDeleteSchema::Restrict),
            outbox_exclude: false,
            rationale: None,
            relation_kind: Some(RelationKindSchema::ForeignKey),
            renamed_from: None,
            sequence_within: None,
            sql_type: "BIGINT".to_string(),
            unique: false,
        }
    }

    fn scalar_column(name: &str) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: name.to_string(),
            nullable: true,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "TEXT".to_string(),
            unique: false,
        }
    }

    fn table_with_fk_count(n: usize) -> TableSchema {
        let mut columns = vec![scalar_column("note")];
        for i in 0..n {
            columns.push(fk_column(&format!("ref_{i}_id"), &format!("ref_{i}")));
        }
        TableSchema {
            app: None,
            columns,
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "edge".to_string(),
            tenant_key: None,
        }
    }

    #[test]
    fn four_fks_emit_eight_steps() {
        let op = SchemaOperation::AddTable(table_with_fk_count(4));
        let steps = MultiFkStaging::emit(&op, &ctx()).unwrap();
        assert_eq!(steps.len(), 8);
    }

    #[test]
    fn step_layout_alternates_expand_and_validate() {
        let op = SchemaOperation::AddTable(table_with_fk_count(4));
        let steps = MultiFkStaging::emit(&op, &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
            let expected = if idx % 2 == 0 {
                StepKind::ExpandSchema
            } else {
                StepKind::ValidateBackfill
            };
            assert_eq!(step.kind, expected);
        }
    }

    #[test]
    fn each_pair_targets_a_distinct_constraint() {
        let op = SchemaOperation::AddTable(table_with_fk_count(4));
        let steps = MultiFkStaging::emit(&op, &ctx()).unwrap();
        let mut names = Vec::new();
        for step in steps.iter().step_by(2) {
            let StepParameters::ExpandSchema { sql_segments } = &step.parameters else {
                panic!("expected ExpandSchema");
            };
            // Each SQL fragment carries the per-FK constraint name —
            // collect them and assert they're all distinct.
            let sql = &sql_segments[0];
            let needle = "CONSTRAINT \"";
            let start = sql.find(needle).unwrap() + needle.len();
            let end = sql[start..].find('"').unwrap() + start;
            names.push(sql[start..end].to_string());
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "distinct constraint names: {names:?}"
        );
    }

    #[test]
    fn three_fks_below_threshold_refused() {
        let op = SchemaOperation::AddTable(table_with_fk_count(3));
        let err = MultiFkStaging::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::CannotEmit { .. }));
    }

    #[test]
    fn rejects_drop_table() {
        let op = SchemaOperation::DropTable("edge".to_string());
        let err = MultiFkStaging::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn each_expand_step_emits_not_valid_clause() {
        let op = SchemaOperation::AddTable(table_with_fk_count(4));
        let steps = MultiFkStaging::emit(&op, &ctx()).unwrap();
        for step in steps.iter().step_by(2) {
            let StepParameters::ExpandSchema { sql_segments } = &step.parameters else {
                panic!("expected ExpandSchema");
            };
            assert!(sql_segments[0].ends_with("NOT VALID"));
        }
    }

    #[test]
    fn each_validate_step_emits_validate_constraint() {
        let op = SchemaOperation::AddTable(table_with_fk_count(4));
        let steps = MultiFkStaging::emit(&op, &ctx()).unwrap();
        for step in steps.iter().skip(1).step_by(2) {
            let StepParameters::ValidateBackfill { gate_query } = &step.parameters else {
                panic!("expected ValidateBackfill");
            };
            assert!(gate_query.contains("VALIDATE CONSTRAINT"));
            assert!(gate_query.contains("\"edge\""));
        }
    }
}
