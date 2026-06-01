//! Index-dependent staged rollout pattern.
//! Covers the "Add index" row of the v3 plan §7 classification table
//! when the index lacks `concurrently = true`. The classifier
//! escalates the operation to `ExpandContract`; this pattern emits
//! the staged plan that rebuilds the index `CONCURRENTLY` and gates
//! on `pg_index.indvalid` before declaring the rollout complete.
//! # Operation shape
//! Accepts [`AddIndex`](SchemaOperation::AddIndex) with any
//! [`requires_out_of_transaction`](crate::migrate::schema::IndexSchema::requires_out_of_transaction)
//! value — the pattern always emits `CONCURRENTLY` regardless. The
//! input flag drives the segment planner; once the operation
//! has reached this pattern the live-plan is committed to the
//! concurrent-build path.
//! # Step graph
//! 1. [`StepKind::ExpandSchema`] — `CREATE INDEX CONCURRENTLY <name>
//! ON <table> (...)`. Runs outside any transaction; the runner's
//! out-of-transaction segment lane handles dispatch.
//! 2. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//! until `SELECT indvalid FROM pg_index WHERE indexrelid =
//! '<name>'::regclass` returns `t`. A `false` result signals the
//! concurrent build failed midway and the index must be dropped
//! and re-created (the runner surfaces an actionable refusal in
//! that case).
//! No backfill is needed — Postgres builds the index asynchronously
//! and `indvalid = true` is the canonical "ready for queries" signal.
//! [`Pattern::IDEMPOTENT_PREDICATE`] is `false` for this pattern
//! because no [`StepKind::BackfillChunked`] step is emitted; the
//! constant exists per-pattern so the cross-pattern dispatch witness
//! in [`super::tests`] can assert "claims chunked backfill" matches
//! "emits chunked backfill".

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::schema::{IndexKindSchema, IndexSchema, IndexTargetSchema};

/// Marker type implementing [`Pattern`] for the concurrent index
/// build sequence.
pub struct IndexDependent;

impl Pattern for IndexDependent {
    const ID: &'static str = "index_dependent";
    const IDEMPOTENT_PREDICATE: bool = false;

    fn emit(op: &SchemaOperation, _ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let index = match op {
            SchemaOperation::AddIndex(index) => index,
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AddIndex".to_string(),
                });
            }
        };

        let create_sql = render_create_index(index)?;
        let gate_query = format!(
            "SELECT indvalid FROM pg_index WHERE indexrelid = '{name}'::regclass",
            name = index.name,
        );

        Ok(vec![
            Step {
                kind: StepKind::ExpandSchema,
                ordinal: 0,
                parameters: StepParameters::ExpandSchema {
                    sql_segments: vec![create_sql],
                },
            },
            Step {
                kind: StepKind::ValidateBackfill,
                ordinal: 1,
                parameters: StepParameters::ValidateBackfill { gate_query },
            },
        ])
    }
}

/// Render the `CREATE INDEX CONCURRENTLY` fragment for the supplied
/// index. Reused by [`super::unique_via_index`] for the unique-index
/// case so the column-list rendering rule lives in one place.
pub(super) fn render_create_index(index: &IndexSchema) -> Result<String, PatternError> {
    let unique = match index.kind {
        IndexKindSchema::NonUnique => "",
        IndexKindSchema::UniqueConstraint | IndexKindSchema::UniqueIndex => "UNIQUE ",
    };
    let target = render_index_target(&index.target)?;
    Ok(format!(
        "CREATE {unique}INDEX CONCURRENTLY {name} ON {table} {target}",
        name = quote_ident(&index.name),
        table = quote_ident(&index.table),
    ))
}

fn render_index_target(target: &IndexTargetSchema) -> Result<String, PatternError> {
    match target {
        IndexTargetSchema::Columns(cols) => {
            if cols.is_empty() {
                return Err(PatternError::Invariant {
                    pattern: IndexDependent::ID,
                    detail: "index target has zero columns".to_string(),
                });
            }
            let mut out = String::from("(");
            for (i, col) in cols.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&quote_ident(&col.name));
            }
            out.push(')');
            Ok(out)
        }
        IndexTargetSchema::Expression(expr) => Ok(format!("({expr})")),
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
        IndexColumnSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexTypeSchema,
    };

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn index(name: &str, kind: IndexKindSchema) -> IndexSchema {
        IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: "vehicle".to_string(),
            target: IndexTargetSchema::Columns(vec![IndexColumnSchema {
                name: "owner_id".to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            }]),
        }
    }

    #[test]
    fn emits_two_step_concurrent_build_sequence() {
        let op = SchemaOperation::AddIndex(index("idx_vehicle_owner", IndexKindSchema::NonUnique));
        let steps = IndexDependent::emit(&op, &ctx()).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::ValidateBackfill);
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let op = SchemaOperation::AddIndex(index("idx_vehicle_owner", IndexKindSchema::NonUnique));
        let steps = IndexDependent::emit(&op, &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn create_index_emits_concurrently_clause() {
        let op = SchemaOperation::AddIndex(index("idx_vehicle_owner", IndexKindSchema::NonUnique));
        let steps = IndexDependent::emit(&op, &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("CONCURRENTLY"));
        assert!(sql_segments[0].contains("\"idx_vehicle_owner\""));
        assert!(sql_segments[0].contains("\"vehicle\""));
        assert!(sql_segments[0].contains("\"owner_id\""));
        assert!(!sql_segments[0].contains("UNIQUE"));
    }

    #[test]
    fn validate_query_targets_pg_index_indvalid() {
        let op = SchemaOperation::AddIndex(index("idx_vehicle_owner", IndexKindSchema::NonUnique));
        let steps = IndexDependent::emit(&op, &ctx()).unwrap();
        let StepParameters::ValidateBackfill { gate_query } = &steps[1].parameters else {
            panic!("expected ValidateBackfill");
        };
        assert!(gate_query.contains("pg_index"));
        assert!(gate_query.contains("indvalid"));
        assert!(gate_query.contains("idx_vehicle_owner"));
    }

    #[test]
    fn rejects_drop_index() {
        let op = SchemaOperation::DropIndex(index("idx_vehicle_owner", IndexKindSchema::NonUnique));
        let err = IndexDependent::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn empty_column_list_is_invariant_violation() {
        let mut idx = index("idx_vehicle_owner", IndexKindSchema::NonUnique);
        idx.target = IndexTargetSchema::Columns(Vec::new());
        let op = SchemaOperation::AddIndex(idx);
        let err = IndexDependent::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::Invariant { .. }));
    }
}
