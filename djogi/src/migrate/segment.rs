//! Segment planning — partitions a lowered [`SchemaDelta`] into an
//! ordered sequence of segments tagged transactional, non-transactional,
//! or metadata-only.
//!
//! # Why segments
//!
//! Postgres lets the runner wrap a sequence of DDL statements in a
//! single transaction so a failure mid-apply rolls back to a clean
//! state. But a handful of operations cannot run inside a transaction
//! at all — `CREATE INDEX CONCURRENTLY` is the canonical example, and
//! Phase 7-Zero's `IndexSpec::requires_out_of_transaction` field
//! captures the operator's intent at descriptor time. The segment
//! planner laddered the lowered SQL into transactional batches with
//! non-transactional segments between them; the runner T4 drives each
//! segment with the matching execution mode.
//!
//! Metadata-only operations ([`SchemaOperation::RenameApp`],
//! [`SchemaOperation::MoveModelBetweenApps`]) do not emit DDL — only
//! folder moves + ledger UPDATEs. They live in their own segment
//! kind so the runner dispatches them to the metadata path instead
//! of the SQL path.
//!
//! # Profile-free
//!
//! Per Phase 7-Zero v3 §6.2 ("Profile-free"): the planner does NOT
//! gate on dev / prod / CI / any other profile. The same input
//! produces the same plan everywhere. The only signal that drives
//! transactional vs non-transactional classification is
//! [`IndexSchema::requires_out_of_transaction`], which originates at
//! descriptor declaration time and travels through the snapshot.
//!
//! # Ordering
//!
//! Within a plan, operations are ordered to satisfy dependency
//! constraints:
//!
//! 1. `AddEnum` runs before any column that references the new type
//!    (we do not emit cross-references in T3 — column types are raw
//!    SQL — so this is a "be safe" ordering, not a strict requirement).
//! 2. `AddTable` runs before `AddForeignKey` referencing it.
//! 3. `RenameTable` runs before any further mutation on the renamed
//!    table.
//! 4. `DropForeignKey` runs before `DropTable` so the table can drop
//!    cleanly. The differ already groups FK drops with the column
//!    they came from, so within a single delta this is mostly
//!    automatic; the planner enforces it across the whole bucket.
//! 5. `DropTable` runs before `DropEnum` whose values appeared only
//!    in the dropped table.
//! 6. Index ops cluster after structural ops on the same table.
//! 7. Metadata-only ops cluster at the end.
//!
//! The full ordering is implemented in [`order_operations`].

use super::diff::{Classification, SchemaDelta, SchemaOperation};
use super::projection::BucketKey;
use super::sql::{OperationSql, SqlEmitError, lower_operation};

/// Top-level migration plan for one bucket.
///
/// Holds the bucket identity, the differ's classification, and the
/// ordered segment sequence. Empty plans (`segments.is_empty()`) are
/// the common case for `Classification::NoOp` deltas; callers should
/// treat an empty plan as "nothing to do".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// Bucket the plan applies to.
    pub bucket: BucketKey,
    /// Differ-side classification — the segment planner does not
    /// re-derive this; it is the same value the differ produced.
    pub classification: Classification,
    /// Ordered segments. Run in order. Each segment's execution mode
    /// is dictated by its [`SegmentKind`].
    pub segments: Vec<Segment>,
}

/// A run of operations sharing the same execution mode.
///
/// The runner walks `segments` in order, dispatching each segment to
/// the matching execution path. Within a transactional segment the
/// runner opens one transaction and executes every statement inside
/// it; within a non-transactional segment each statement runs
/// outside any transaction; within a metadata-only segment no SQL
/// runs at all (folder moves + ledger UPDATEs happen via T6 / T4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// How the runner should execute this segment.
    pub kind: SegmentKind,
    /// Lowered operation SQL pairs in execution order. Empty
    /// segments are never produced — the planner skips empty
    /// segments at construction time.
    pub statements: Vec<OperationSql>,
}

/// Execution mode tag for a [`Segment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Runs inside the per-migration transaction. The runner opens
    /// one BEGIN / COMMIT pair around every statement in the
    /// segment.
    Transactional,
    /// Runs outside any transaction. Each statement is its own
    /// implicit transaction (Postgres autocommit). Required for
    /// `CREATE INDEX CONCURRENTLY` and any other DDL that Postgres
    /// rejects inside a transaction.
    NonTransactional,
    /// No SQL runs. Metadata-only operations
    /// ([`SchemaOperation::RenameApp`],
    /// [`SchemaOperation::MoveModelBetweenApps`]) emit comment
    /// placeholders via T3's SQL emitter so the migration file is
    /// self-documenting; the runner reads the segment kind and
    /// dispatches to the metadata path (folder rename + ledger
    /// UPDATE). Carrying the SQL placeholder text lets `migrations
    /// status` print a meaningful diff line.
    MetadataOnly,
}

impl Segment {
    /// Convenience constructor — `Some(segment)` when the statements
    /// list is non-empty, `None` otherwise. Used by the planner to
    /// avoid pushing empty segments.
    fn new_if_non_empty(kind: SegmentKind, statements: Vec<OperationSql>) -> Option<Segment> {
        if statements.is_empty() {
            None
        } else {
            Some(Segment { kind, statements })
        }
    }
}

/// Plan a [`SchemaDelta`] into an ordered sequence of segments.
///
/// **Pure function.** No I/O; no env reads; no profile gating per
/// Phase 7-Zero v3 §6.2. Same input always produces the same output.
///
/// **Hard-error surfaces.** Returns the same errors as
/// [`crate::migrate::sql::lower_delta`] — `Unsupported` and
/// `PkTypeFlipMustRouteToT9` propagate up. The planner does not try
/// to recover from a hard error; Phase 7's "fail loudly" stance
/// applies.
///
/// **No-op short-circuit.** A delta with `Classification::NoOp`
/// returns a [`MigrationPlan`] with an empty `segments` vector. The
/// runner short-circuits when it sees an empty plan.
///
/// `clippy::result_large_err` is silenced because [`SqlEmitError`]
/// is a structural error type — see the matching note on
/// [`crate::migrate::sql::lower_delta`].
#[allow(clippy::result_large_err)]
pub fn plan_delta(delta: &SchemaDelta) -> Result<MigrationPlan, SqlEmitError> {
    if matches!(delta.classification, Classification::NoOp) {
        return Ok(MigrationPlan {
            bucket: delta.bucket.clone(),
            classification: delta.classification.clone(),
            segments: Vec::new(),
        });
    }

    // Step 1 — order the operations into a dependency-respecting
    // sequence. Ordering is independent of segment classification.
    let ordered = order_operations(&delta.operations);

    // Step 2 — walk in order, lowering each operation and dropping
    // it into the segment whose kind matches. Adjacent operations
    // of the same kind coalesce into one segment.
    let mut segments: Vec<Segment> = Vec::new();
    let mut current_kind: Option<SegmentKind> = None;
    let mut current_stmts: Vec<OperationSql> = Vec::new();

    for op in ordered {
        let kind = classify_operation(op);
        let lowered = lower_operation(op)?;
        match current_kind {
            Some(k) if k == kind => current_stmts.push(lowered),
            _ => {
                if let Some(seg) = Segment::new_if_non_empty(
                    current_kind.unwrap_or(SegmentKind::Transactional),
                    std::mem::take(&mut current_stmts),
                ) {
                    segments.push(seg);
                }
                current_kind = Some(kind);
                current_stmts.push(lowered);
            }
        }
    }
    if let Some(seg) = Segment::new_if_non_empty(
        current_kind.unwrap_or(SegmentKind::Transactional),
        current_stmts,
    ) {
        segments.push(seg);
    }

    Ok(MigrationPlan {
        bucket: delta.bucket.clone(),
        classification: delta.classification.clone(),
        segments,
    })
}

/// Classify a [`SchemaOperation`] into a [`SegmentKind`].
///
/// Decision rules (in order):
///
/// 1. [`SchemaOperation::RenameApp`] /
///    [`SchemaOperation::MoveModelBetweenApps`] -> `MetadataOnly`.
/// 2. [`SchemaOperation::AddIndex`] /
///    [`SchemaOperation::DropIndex`] whose
///    `IndexSchema::requires_out_of_transaction == true` ->
///    `NonTransactional`.
/// 3. Everything else -> `Transactional`.
///
/// `PkTypeFlip` and `Unsupported` would error out before reaching
/// this fn (the planner calls `lower_operation` after this fn picks
/// the kind, but `lower_operation` is what surfaces the error — so
/// the kind we pick for `PkTypeFlip` / `Unsupported` is structurally
/// irrelevant). We pick `Transactional` as the safe default for
/// those two variants.
pub(crate) fn classify_operation(op: &SchemaOperation) -> SegmentKind {
    match op {
        SchemaOperation::RenameApp { .. } | SchemaOperation::MoveModelBetweenApps { .. } => {
            SegmentKind::MetadataOnly
        }
        SchemaOperation::AddIndex(idx) | SchemaOperation::DropIndex(idx)
            if idx.requires_out_of_transaction =>
        {
            SegmentKind::NonTransactional
        }
        _ => SegmentKind::Transactional,
    }
}

/// Re-order operations into a dependency-respecting sequence.
///
/// Stable sort by an integer "phase" — operations with lower phase
/// numbers run first, ties preserve input order. This keeps the
/// planner deterministic without an explicit dependency graph (the
/// dependency graph is implicit in the phase ordering).
///
/// **Phases:**
///
/// | Phase | Operations |
/// |-------|------------|
/// |  0    | `AddEnum` |
/// |  1    | `AddTable` |
/// |  2    | `RenameTable` |
/// |  3    | `AddColumn`, `RenameColumn`, `AlterColumn`, `AddForeignKey` |
/// |  4    | `AddEnumVariant` |
/// |  5    | `AddIndex` (transactional) |
/// |  5    | `AddIndex` (non-transactional) — same phase as transactional; the segment classifier separates them |
/// |  6    | `DropIndex` |
/// |  7    | `DropForeignKey` |
/// |  8    | `DropColumn` |
/// |  9    | `DropTable` |
/// | 10    | `DropEnum` |
/// | 11    | `RenameApp`, `MoveModelBetweenApps` (metadata) |
/// | 12    | `Unsupported`, `PkTypeFlip` (will error during lowering) |
///
/// The phasing keeps adds before drops, structural changes before
/// index changes, and metadata at the end. Within a phase, input
/// order is preserved — the differ already grouped per-table column
/// changes together, and the planner does not break that grouping.
fn order_operations(ops: &[SchemaOperation]) -> Vec<&SchemaOperation> {
    let mut tagged: Vec<(usize, usize, &SchemaOperation)> = ops
        .iter()
        .enumerate()
        .map(|(i, op)| (operation_phase(op), i, op))
        .collect();
    tagged.sort_by_key(|(phase, idx, _)| (*phase, *idx));
    tagged.into_iter().map(|(_, _, op)| op).collect()
}

fn operation_phase(op: &SchemaOperation) -> usize {
    match op {
        SchemaOperation::AddEnum(_) => 0,
        SchemaOperation::AddTable(_) => 1,
        SchemaOperation::RenameTable { .. } => 2,
        SchemaOperation::AddColumn { .. }
        | SchemaOperation::RenameColumn { .. }
        | SchemaOperation::AlterColumn { .. }
        | SchemaOperation::AddForeignKey { .. } => 3,
        SchemaOperation::AddEnumVariant { .. } => 4,
        SchemaOperation::AddIndex(_) => 5,
        SchemaOperation::DropIndex(_) => 6,
        SchemaOperation::DropForeignKey { .. } => 7,
        SchemaOperation::DropColumn { .. } => 8,
        SchemaOperation::DropTable(_) => 9,
        SchemaOperation::DropEnum(_) => 10,
        SchemaOperation::RenameApp { .. } | SchemaOperation::MoveModelBetweenApps { .. } => 11,
        // Hard-error variants — phase value is unreachable in
        // practice because lower_operation will fail before any
        // segment is materialized. Pin to the end so a future
        // refactor that ignores the error still puts them in a
        // sensible spot.
        SchemaOperation::PkTypeFlip { .. } | SchemaOperation::Unsupported { .. } => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::diff::{ColumnChange, SchemaDelta};
    use crate::migrate::projection::BucketKey;
    use crate::migrate::schema::{
        ColumnSchema, EnumSchema, IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema,
        IndexOrderSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema, PkKindSchema,
        PrimaryKeySchema, TableSchema,
    };

    fn bucket() -> BucketKey {
        BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        }
    }

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: name.to_string(),
            nullable,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: ty.to_string(),
            unique: false,
        }
    }

    fn id_column_heerid() -> ColumnSchema {
        ColumnSchema {
            default_sql: Some("generate_id()".to_string()),
            ..col("id", "BIGINT", false)
        }
    }

    fn synth_table(name: &str) -> TableSchema {
        TableSchema {
            app: None,
            columns: vec![id_column_heerid(), col("name", "TEXT", true)],
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
            table: name.to_string(),
            tenant_key: None,
        }
    }

    fn idx(name: &str, table: &str, cols: &[&str]) -> IndexSchema {
        IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::NonUnique,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: table.to_string(),
            target: IndexTargetSchema::Columns(
                cols.iter()
                    .map(|c| IndexColumnSchema {
                        name: (*c).to_string(),
                        nulls: IndexNullsOrderSchema::Default,
                        opclass: None,
                        order: IndexOrderSchema::Asc,
                    })
                    .collect(),
            ),
        }
    }

    // ── No-op + empty deltas ─────────────────────────────────────────

    #[test]
    fn noop_delta_yields_empty_plan() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: Vec::new(),
            classification: Classification::NoOp,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert!(plan.segments.is_empty());
        assert_eq!(plan.bucket, bucket());
        assert_eq!(plan.classification, Classification::NoOp);
    }

    // ── Single transactional segment ─────────────────────────────────

    #[test]
    fn single_add_table_lands_in_one_transactional_segment() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::AddTable(synth_table("users"))],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
        assert_eq!(plan.segments[0].statements.len(), 1);
    }

    // ── Non-transactional segments ───────────────────────────────────

    #[test]
    fn concurrent_index_lands_in_non_transactional_segment() {
        let mut i = idx("users_email_idx", "users", &["email"]);
        i.requires_out_of_transaction = true;
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::AddIndex(i)],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::NonTransactional);
    }

    #[test]
    fn mixed_transactional_and_non_transactional_split_into_two_segments() {
        let mut concurrent_idx = idx("users_email_idx", "users", &["email"]);
        concurrent_idx.requires_out_of_transaction = true;
        let plain_idx = idx("users_name_idx", "users", &["name"]);
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(synth_table("users")),
                SchemaOperation::AddIndex(plain_idx),
                SchemaOperation::AddIndex(concurrent_idx),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("ok");
        // Phase ordering: AddTable (phase 1), then both AddIndex
        // (phase 5) in input order. Same phase, but different
        // SegmentKind because one is concurrent.
        assert_eq!(plan.segments.len(), 2);
        assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
        assert!(plan.segments[0].statements.len() >= 2); // AddTable + plain_idx
        assert_eq!(plan.segments[1].kind, SegmentKind::NonTransactional);
    }

    #[test]
    fn drop_index_concurrently_lands_in_non_transactional_segment() {
        let mut i = idx("users_email_idx", "users", &["email"]);
        i.requires_out_of_transaction = true;
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::DropIndex(i)],
            classification: Classification::Destructive,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::NonTransactional);
    }

    // ── Metadata-only segments ───────────────────────────────────────

    #[test]
    fn rename_app_lands_in_metadata_only_segment() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::RenameApp {
                from: "old".to_string(),
                to: "new".to_string(),
            }],
            classification: Classification::Reversible,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::MetadataOnly);
        // The metadata-only segment still carries the SQL comment so
        // `migrations status` and the migration file have something
        // to print.
        assert!(plan.segments[0].statements[0].up.contains("METADATA-ONLY"));
    }

    #[test]
    fn move_model_between_apps_lands_in_metadata_only_segment() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::MoveModelBetweenApps {
                model: "users".to_string(),
                from_app: "old".to_string(),
                to_app: "new".to_string(),
            }],
            classification: Classification::Reversible,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::MetadataOnly);
    }

    // ── Ordering ─────────────────────────────────────────────────────

    #[test]
    fn add_enum_runs_before_add_table() {
        // Even when the operations come in reversed input order,
        // the planner sorts AddEnum (phase 0) before AddTable
        // (phase 1).
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(synth_table("users")),
                SchemaOperation::AddEnum(EnumSchema {
                    name: "status".to_string(),
                    variants: vec!["active".to_string()],
                }),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("ok");
        // Both same kind (transactional) so they coalesce into one
        // segment. AddEnum (label "AddEnum status") must precede
        // AddTable.
        assert_eq!(plan.segments.len(), 1);
        let labels: Vec<_> = plan.segments[0]
            .statements
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        let enum_pos = labels
            .iter()
            .position(|l| l.starts_with("AddEnum"))
            .unwrap();
        let table_pos = labels
            .iter()
            .position(|l| l.starts_with("AddTable"))
            .unwrap();
        assert!(enum_pos < table_pos, "labels: {labels:?}");
    }

    #[test]
    fn drop_table_runs_before_drop_enum() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::DropEnum("status".to_string()),
                SchemaOperation::DropTable("users".to_string()),
            ],
            classification: Classification::Destructive,
        };
        let plan = plan_delta(&delta).expect("ok");
        let labels: Vec<_> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        let table_pos = labels
            .iter()
            .position(|l| l.starts_with("DropTable"))
            .unwrap();
        let enum_pos = labels
            .iter()
            .position(|l| l.starts_with("DropEnum"))
            .unwrap();
        assert!(table_pos < enum_pos, "labels: {labels:?}");
    }

    #[test]
    fn rename_table_runs_before_alter_column_on_renamed_table() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AlterColumn {
                    table: "gadgets".to_string(),
                    column: "name".to_string(),
                    change: ColumnChange::SetNullable(false),
                },
                SchemaOperation::RenameTable {
                    from: "widgets".to_string(),
                    to: "gadgets".to_string(),
                },
            ],
            classification: Classification::Reversible,
        };
        let plan = plan_delta(&delta).expect("ok");
        let labels: Vec<_> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        let rename_pos = labels
            .iter()
            .position(|l| l.starts_with("RenameTable"))
            .unwrap();
        let alter_pos = labels
            .iter()
            .position(|l| l.starts_with("AlterColumn"))
            .unwrap();
        assert!(rename_pos < alter_pos, "labels: {labels:?}");
    }

    #[test]
    fn drop_foreign_key_runs_before_drop_column() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::DropColumn {
                    table: "posts".to_string(),
                    column: "author_id".to_string(),
                },
                SchemaOperation::DropForeignKey {
                    table: "posts".to_string(),
                    column: "author_id".to_string(),
                },
            ],
            classification: Classification::Destructive,
        };
        let plan = plan_delta(&delta).expect("ok");
        let labels: Vec<_> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        let fk_pos = labels
            .iter()
            .position(|l| l.starts_with("DropForeignKey"))
            .unwrap();
        let col_pos = labels
            .iter()
            .position(|l| l.starts_with("DropColumn"))
            .unwrap();
        assert!(fk_pos < col_pos, "labels: {labels:?}");
    }

    // ── Hard-error surfaces ──────────────────────────────────────────

    #[test]
    fn pk_type_flip_in_delta_errors_during_planning() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::PkTypeFlip {
                table: "users".to_string(),
                from: PkKindSchema::HeerId,
                to: PkKindSchema::HeerIdRecencyBiased,
            }],
            classification: Classification::PkTypeFlip {
                co_destructive: false,
                co_lossy: false,
            },
        };
        let err = plan_delta(&delta).expect_err("must error");
        assert!(matches!(err, SqlEmitError::PkTypeFlipMustRouteToT9 { .. }));
    }

    #[test]
    fn unsupported_in_delta_errors_during_planning() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::Unsupported {
                reason: "partition method change".to_string(),
            }],
            classification: Classification::Unsupported {
                reason: "partition method change".to_string(),
            },
        };
        let err = plan_delta(&delta).expect_err("must error");
        assert!(matches!(err, SqlEmitError::Unsupported { .. }));
    }

    // ── Determinism ──────────────────────────────────────────────────

    #[test]
    fn same_delta_plans_byte_identically() {
        let mut concurrent_idx = idx("users_email_idx", "users", &["email"]);
        concurrent_idx.requires_out_of_transaction = true;
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(synth_table("users")),
                SchemaOperation::AddIndex(concurrent_idx),
            ],
            classification: Classification::Additive,
        };
        let a = plan_delta(&delta).unwrap();
        let b = plan_delta(&delta).unwrap();
        assert_eq!(a, b);
    }

    // ── Coalescing ───────────────────────────────────────────────────

    #[test]
    fn adjacent_same_kind_operations_coalesce() {
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(synth_table("users")),
                SchemaOperation::AddTable(synth_table("posts")),
                SchemaOperation::AddTable(synth_table("comments")),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("ok");
        // All three transactional -> one segment with three
        // statements.
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].statements.len(), 3);
    }

    #[test]
    fn alternating_kinds_produce_alternating_segments() {
        let mut c1 = idx("a_idx", "a", &["x"]);
        c1.requires_out_of_transaction = true;
        let mut c2 = idx("b_idx", "b", &["y"]);
        c2.requires_out_of_transaction = true;
        // Two non-transactional indexes wrapped around a metadata
        // op. Phase ordering puts AddIndex before RenameApp, so the
        // metadata op lands at the end — expect [non_tx,
        // metadata_only].
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddIndex(c1),
                SchemaOperation::AddIndex(c2),
                SchemaOperation::RenameApp {
                    from: "old".to_string(),
                    to: "new".to_string(),
                },
            ],
            classification: Classification::Reversible,
        };
        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 2);
        assert_eq!(plan.segments[0].kind, SegmentKind::NonTransactional);
        assert_eq!(plan.segments[0].statements.len(), 2);
        assert_eq!(plan.segments[1].kind, SegmentKind::MetadataOnly);
    }
}
