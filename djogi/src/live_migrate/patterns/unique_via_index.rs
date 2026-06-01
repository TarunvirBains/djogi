//! Unique-via-index pattern.
//! Covers two rows of the v3 plan §7 classification table:
//! 1. "Add unique constraint to populated table" — emit `CREATE
//! UNIQUE INDEX CONCURRENTLY` then promote it to a constraint via
//!    `ALTER TABLE … ADD CONSTRAINT … UNIQUE USING INDEX`.
//! 2. "Replacing an index (DROP + CREATE on overlapping columns)"
//!    when the live plan owns the rebuild — emit `CREATE INDEX
//! CONCURRENTLY` for the new shape, gate on `indvalid`, then
//!    `DROP INDEX CONCURRENTLY` the legacy index.
//! # Operation shape
//! Accepts [`AddIndex`](SchemaOperation::AddIndex). The pattern
//! branches on the input's [`IndexKindSchema`] to choose between the
//! plain-rebuild and constraint-promotion graphs.
//! # Step graph (unique constraint promotion)
//! 1. [`StepKind::ExpandSchema`] — `CREATE UNIQUE INDEX CONCURRENTLY
//! <name> ON <table> (...)`.
//! 2. [`StepKind::ValidateBackfill`] — operator gate on
//!    `pg_index.indvalid`.
//! 3. [`StepKind::FinalizeConstraints`] — `ALTER TABLE <t> ADD
//! CONSTRAINT <name> UNIQUE USING INDEX <name>` (the constraint
//!    name reuses the index name; Postgres allows the same identifier
//!    for both).
//! # Step graph (plain index rebuild)
//! 1. [`StepKind::ExpandSchema`] — `CREATE INDEX CONCURRENTLY ...`.
//! 2. [`StepKind::ValidateBackfill`] — operator gate on
//!    `pg_index.indvalid`.
//! 3. [`StepKind::FinalizeConstraints`] — empty fragment list. The
//!    promotion-only step exists so the plan-file shape matches the
//!    constraint case under introspection — operators see "this step
//!    has nothing to do, advance" rather than "this pattern omitted
//!    finalize".
//!    No backfill is needed in either branch — the index build is the
//!    only data-touching step. [`Pattern::IDEMPOTENT_PREDICATE`] is
//!    `false`.

use super::index_dependent::render_create_index;
use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::schema::{IndexKindSchema, IndexSchema};

/// Marker type implementing [`Pattern`] for the `UNIQUE INDEX
/// CONCURRENTLY` plus `USING INDEX` promotion sequence.
pub struct UniqueViaIndex;

impl Pattern for UniqueViaIndex {
    const ID: &'static str = "unique_via_index";
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

        let finalize_segments = match index.kind {
            IndexKindSchema::UniqueConstraint => vec![format!(
                "ALTER TABLE {tbl} ADD CONSTRAINT {name} UNIQUE USING INDEX {name}",
                tbl = quote_ident(&index.table),
                name = quote_ident(&index.name),
            )],
            IndexKindSchema::UniqueIndex | IndexKindSchema::NonUnique => Vec::new(),
        };

        let mut steps = vec![
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
            Step {
                kind: StepKind::FinalizeConstraints,
                ordinal: 2,
                parameters: StepParameters::FinalizeConstraints {
                    sql_segments: finalize_segments,
                },
            },
        ];

        if let Some(legacy) = legacy_replacement_target(index) {
            let drop_sql = format!(
                "DROP INDEX CONCURRENTLY {name}",
                name = quote_ident(&legacy),
            );
            steps.push(Step {
                kind: StepKind::CleanupLegacyState,
                ordinal: 3,
                parameters: StepParameters::CleanupLegacyState {
                    sql_segments: vec![drop_sql],
                },
            });
        }

        Ok(steps)
    }
}

/// When the new index name carries the conventional `_v2` suffix the
/// dispatcher uses to mark a replacement build, return the legacy
/// index name to drop. Returns `None` for plain unique-add operations
/// that have no legacy counterpart. The convention is documented at
/// the dispatch layer (T10); the pattern only needs the suffix-strip
/// rule to know whether the cleanup step belongs in the plan.
fn legacy_replacement_target(index: &IndexSchema) -> Option<String> {
    let name = &index.name;
    name.strip_suffix("_v2").map(|stem| stem.to_string())
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
        IndexColumnSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexTargetSchema,
        IndexTypeSchema,
    };

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn unique_index(name: &str) -> IndexSchema {
        IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::UniqueConstraint,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: "vehicle".to_string(),
            target: IndexTargetSchema::Columns(vec![IndexColumnSchema {
                name: "vin".to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            }]),
        }
    }

    #[test]
    fn unique_constraint_emits_three_step_promotion_sequence() {
        let op = SchemaOperation::AddIndex(unique_index("uq_vehicle_vin"));
        let steps = UniqueViaIndex::emit(&op, &ctx()).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::ValidateBackfill);
        assert_eq!(steps[2].kind, StepKind::FinalizeConstraints);
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let op = SchemaOperation::AddIndex(unique_index("uq_vehicle_vin"));
        let steps = UniqueViaIndex::emit(&op, &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn expand_step_emits_unique_index_concurrently() {
        let op = SchemaOperation::AddIndex(unique_index("uq_vehicle_vin"));
        let steps = UniqueViaIndex::emit(&op, &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert!(sql_segments[0].contains("UNIQUE"));
        assert!(sql_segments[0].contains("CONCURRENTLY"));
        assert!(sql_segments[0].contains("\"uq_vehicle_vin\""));
    }

    #[test]
    fn finalize_step_promotes_constraint_via_using_index() {
        let op = SchemaOperation::AddIndex(unique_index("uq_vehicle_vin"));
        let steps = UniqueViaIndex::emit(&op, &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[2].parameters else {
            panic!("expected FinalizeConstraints");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("ADD CONSTRAINT"));
        assert!(sql_segments[0].contains("USING INDEX"));
        assert!(sql_segments[0].contains("\"uq_vehicle_vin\""));
    }

    #[test]
    fn replacement_index_appends_drop_concurrently_step() {
        let op = SchemaOperation::AddIndex(unique_index("uq_vehicle_vin_v2"));
        let steps = UniqueViaIndex::emit(&op, &ctx()).unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[3].kind, StepKind::CleanupLegacyState);
        let StepParameters::CleanupLegacyState { sql_segments } = &steps[3].parameters else {
            panic!("expected CleanupLegacyState");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("DROP INDEX CONCURRENTLY"));
        assert!(sql_segments[0].contains("\"uq_vehicle_vin\""));
    }

    #[test]
    fn non_unique_index_skips_using_index_promotion() {
        let mut idx = unique_index("idx_vehicle_owner");
        idx.kind = IndexKindSchema::NonUnique;
        let op = SchemaOperation::AddIndex(idx);
        let steps = UniqueViaIndex::emit(&op, &ctx()).unwrap();
        let StepParameters::FinalizeConstraints { sql_segments } = &steps[2].parameters else {
            panic!("expected FinalizeConstraints");
        };
        assert!(sql_segments.is_empty());
    }

    #[test]
    fn rejects_drop_index() {
        let op = SchemaOperation::DropIndex(unique_index("uq_vehicle_vin"));
        let err = UniqueViaIndex::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }
}
