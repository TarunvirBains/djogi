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

use std::collections::{BTreeMap, BTreeSet};

use super::compose::{
    date_array_helper_operation, numeric_array_helper_operation, requires_date_array_helper,
    requires_numeric_array_helper, requires_tstz_array_helper, tstz_array_helper_operation,
};
use super::diff::{Classification, SchemaDelta, SchemaOperation};
use super::projection::BucketKey;
use super::schema::TableSchema;
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

    // T9 fast-path: a delta carrying a `PkTypeFlipGroup` /
    // `PkTypeFlipMultiGroup` consumes the entire migration (whole-
    // migration non-transactional, per 7-Zero §6.2 deterministic
    // A). Route to the dedicated multi-segment emitter and ignore
    // the standard per-operation path.
    //
    // **Single-parent groups** (`PkTypeFlipGroup`) lower as
    // back-to-back 5-segment plans in input order — correct because
    // single-parent groups never reference partner shadow columns.
    //
    // **Multi-parent groups** (`PkTypeFlipMultiGroup`) lower as ONE
    // stage-interleaved 5-segment plan — at each
    // stage, every member group's stage-N statements are emitted
    // together. Required because the cross-flipping join-table FKs
    // at stage 3b reference shadow columns on every parent, and
    // those shadow columns must already exist (i.e. every parent's
    // stage 1 must have run) before the FK statements run.
    let mut group_segments: Vec<Segment> = Vec::new();
    for op in &delta.operations {
        match op {
            SchemaOperation::PkTypeFlipGroup(g) => {
                group_segments.extend(
                    super::pk_flip::build_segments(g).map_err(|e| SqlEmitError::Diff(e.into()))?,
                );
            }
            SchemaOperation::PkTypeFlipMultiGroup(groups) => {
                group_segments.extend(
                    super::pk_flip::build_segments_multi(groups).map_err(SqlEmitError::Diff)?,
                );
            }
            _ => {}
        }
    }
    if !group_segments.is_empty() {
        return Ok(MigrationPlan {
            bucket: delta.bucket.clone(),
            classification: delta.classification.clone(),
            segments: group_segments,
        });
    }

    // Step 1 — order the operations into a dependency-respecting
    // sequence. Ordering is independent of segment classification.
    let ordered = order_operations(&delta.operations);

    // Step 2 — walk in order, lowering each operation and dropping
    // it into the segment whose kind matches. Adjacent operations
    // of the same kind coalesce into one segment.
    let mut segments: Vec<Segment> = Vec::new();
    let mut lowered_ops: Vec<OperationSql> = Vec::with_capacity(ordered.len());
    let mut lowered_kinds: Vec<SegmentKind> = Vec::with_capacity(ordered.len());

    for op in &ordered {
        let kind = classify_operation(op);
        let lowered = lower_operation(op)?;
        lowered_ops.push(lowered);
        lowered_kinds.push(kind);
    }

    // Inject helper preludes in the same order that compose_up_text /
    // compose_down_text emits them: numeric → date → tstz.  Building a
    // prefix vector and prepending it in one step is critical; using
    // repeated `insert(0, …)` reverses the sequence — each new insert
    // pushes all prior entries down by one, so the last-inserted op ends
    // at index 0 and the first-inserted op ends at the back of the
    // prefix.  The compose_up_text order is the canonical reference
    // because it determines the on-disk SQL file that operators review.
    let mut helper_ops: Vec<OperationSql> = Vec::new();
    let mut helper_kinds: Vec<SegmentKind> = Vec::new();
    if requires_numeric_array_helper(&lowered_ops) {
        helper_ops.push(numeric_array_helper_operation());
        helper_kinds.push(SegmentKind::Transactional);
    }
    if requires_date_array_helper(&lowered_ops) {
        helper_ops.push(date_array_helper_operation());
        helper_kinds.push(SegmentKind::Transactional);
    }
    if requires_tstz_array_helper(&lowered_ops) {
        helper_ops.push(tstz_array_helper_operation());
        helper_kinds.push(SegmentKind::Transactional);
    }
    if !helper_ops.is_empty() {
        helper_ops.extend(lowered_ops);
        helper_kinds.extend(lowered_kinds);
        lowered_ops = helper_ops;
        lowered_kinds = helper_kinds;
    }

    let mut current_kind: Option<SegmentKind> = None;
    let mut current_stmts: Vec<OperationSql> = Vec::new();

    for (kind, op) in lowered_kinds.into_iter().zip(lowered_ops) {
        match current_kind {
            Some(k) if k == kind => current_stmts.push(op),
            _ => {
                if let Some(seg) = Segment::new_if_non_empty(
                    current_kind.unwrap_or(SegmentKind::Transactional),
                    std::mem::take(&mut current_stmts),
                ) {
                    segments.push(seg);
                }
                current_kind = Some(kind);
                current_stmts.push(op);
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
/// |  1    | `RenameTable` |
/// |  2    | `AddTable` |
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
/// **Why `RenameTable` precedes `AddTable`.** A rename is always a
/// "make existing table available under its new name" op. Once renames
/// are applied, every subsequent op (including an `AddTable` whose
/// inline FK targets the post-rename name) can refer to the renamed
/// table without ordering tricks. The prior layout (RenameTable in phase 2,
/// AddTable in phase 1) had issues:
/// when a delta carried `RenameTable users → members` together with
/// `AddTable comments` whose `comments.user_id REFERENCES "members"`,
/// the toposort treated `"members"` as external (not in the AddTable
/// batch) and the emitter wrote the `CREATE TABLE comments` —
/// inlining `REFERENCES "members"` — BEFORE the rename ran. Postgres
/// rejected with "relation does not exist". Hoisting `RenameTable`
/// ahead of `AddTable` removes rename-awareness from the toposort
/// entirely; the new name is just there by the time the inlining
/// resolves.
///
/// The phasing keeps adds before drops, structural changes before
/// index changes, and metadata at the end. Within a phase, input
/// order is preserved — the differ already grouped per-table column
/// changes together, and the planner does not break that grouping.
///
/// **Phase 2 — `AddTable` toposort.** The `CREATE TABLE` emitter
/// inlines `REFERENCES` clauses for FK columns, so a table that
/// references another table must be created AFTER its target.
/// Within phase 2 the planner runs Kahn's algorithm over the
/// FK-dependency graph (edges: `T1 -> T2` when `T1` has an inline FK
/// pointing at `T2`) and emits tables in the resulting topo order
/// instead of input / alphabetical order. **Cycles** (rare —
/// mutually-referencing tables) are broken by emitting every cycle
/// member's `AddTable` *without* its inline FKs and following up
/// with standalone `AddForeignKey` operations after the table batch.
/// This matches the Phase 7-Zero v3 plan's "fail loudly when we
/// can't, but never silently produce DDL Postgres rejects" stance.
/// The prior phase-only sort would emit cross-referencing tables in
/// alphabetical order (because the differ feeds them via `BTreeMap`),
/// causing Postgres to reject
/// the migration with "relation does not exist".
///
/// Returned ops are owned because the cycle-breaking path needs to
/// rewrite `AddTable` payloads (strip inline FKs) and synthesise new
/// `AddForeignKey` ops; the slice-of-refs shape would not allow that.
fn order_operations(ops: &[SchemaOperation]) -> Vec<SchemaOperation> {
    let mut tagged: Vec<(usize, usize, SchemaOperation)> = ops
        .iter()
        .enumerate()
        .map(|(i, op)| (operation_phase(op), i, op.clone()))
        .collect();
    tagged.sort_by_key(|(phase, idx, _)| (*phase, *idx));

    // Split the sorted stream so we can reorder phase 2 (`AddTable`)
    // through the topo-sort. Other phases are already in deterministic
    // order from the stable sort above. Phase 0 (`AddEnum`) and phase
    // 1 (`RenameTable`) flow into `head` so renames apply before any
    // `CREATE TABLE` that may inline a `REFERENCES <renamed>` clause.
    let mut head: Vec<SchemaOperation> = Vec::with_capacity(tagged.len());
    let mut add_tables: Vec<TableSchema> = Vec::new();
    let mut tail: Vec<SchemaOperation> = Vec::with_capacity(tagged.len());
    for (phase, _, op) in tagged {
        match (phase, op) {
            (2, SchemaOperation::AddTable(t)) => add_tables.push(t),
            (p, op) if p < 2 => head.push(op),
            (_, op) => tail.push(op),
        }
    }

    let (toposorted, follow_up_fks) = toposort_add_tables(add_tables);

    let mut out =
        Vec::with_capacity(head.len() + toposorted.len() + follow_up_fks.len() + tail.len());
    out.extend(head);
    out.extend(toposorted.into_iter().map(SchemaOperation::AddTable));
    out.extend(follow_up_fks);
    out.extend(tail);
    out
}

/// Topo-sort a batch of `AddTable` operations by their inline FK
/// dependencies.
///
/// Returns `(ordered_tables, follow_up_fk_ops)`:
///
/// - `ordered_tables` — the input tables, ordered so each table
///   emits AFTER every table its inline FKs reference. Tables with
///   no inline FKs (and no reverse references from cycle-breaking)
///   keep their alphabetical order from the input.
/// - `follow_up_fk_ops` — `AddForeignKey` operations synthesised when
///   a cycle was broken. Empty in the common acyclic case.
///
/// Algorithm: Kahn's. We build an adjacency map keyed by table name,
/// with edges `dependent -> dependency`, plus reverse edges so we
/// can decrement in-degrees. We pop tables with zero in-degree in
/// **alphabetical order** for determinism (tied nodes in Kahn's
/// algorithm are arbitrary; pinning to alphabetical keeps the same
/// input always producing the same output).
///
/// When a cycle is detected (some tables remain with non-zero
/// in-degree after processing all zero-in-degree starts), we break
/// it surgically: every cycle-member table emits without its inline
/// FK columns, and the stripped FKs become standalone
/// `AddForeignKey` ops that run AFTER all `CREATE TABLE` statements.
/// Cycle members emit in alphabetical order; non-cycle tables that
/// happen to land in `tail` because they were starved of in-degree
/// also emit in alphabetical order (a structural property of Kahn's
/// when ties are broken alphabetically).
///
/// Ignored edges:
/// - **Self-references.** A table whose FK points at itself is
///   admissible Postgres DDL — the inline `REFERENCES same_table`
///   is fine because the table exists by the time the constraint
///   check runs. We drop these edges so they never trigger the
///   cycle-breaker.
/// - **External references.** An FK pointing at a table NOT in this
///   batch (e.g., a table that already exists in the live schema)
///   cannot constrain ordering within the batch — drop the edge.
fn toposort_add_tables(tables: Vec<TableSchema>) -> (Vec<TableSchema>, Vec<SchemaOperation>) {
    if tables.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Build name → table map (alphabetical iteration via BTreeMap).
    let mut by_name: BTreeMap<String, TableSchema> = BTreeMap::new();
    for t in tables {
        by_name.insert(t.table.clone(), t);
    }
    let in_batch: BTreeSet<String> = by_name.keys().cloned().collect();

    // Edges: dependent → set of dependencies (tables that must be
    // created before `dependent`). Self-loops and out-of-batch
    // references are filtered out.
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, t) in &by_name {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for col in &t.columns {
            if let Some(fk) = &col.foreign_key
                && fk.ref_table != *name
                && in_batch.contains(&fk.ref_table)
            {
                set.insert(fk.ref_table.clone());
            }
        }
        deps.insert(name.clone(), set);
    }

    // Reverse adjacency for in-degree decrementing — `reverse[T]` is
    // every table that depends on `T`.
    let mut reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in by_name.keys() {
        reverse.insert(name.clone(), BTreeSet::new());
    }
    for (dependent, deps_of) in &deps {
        for dependency in deps_of {
            reverse
                .get_mut(dependency)
                .expect("reverse entry exists for every batch table")
                .insert(dependent.clone());
        }
    }

    // Kahn's queue, deterministic via BTreeSet iteration order.
    // Initialise with every table whose in-degree is zero — i.e.
    // every table with an empty deps set.
    let mut ready: BTreeSet<String> = deps
        .iter()
        .filter_map(|(n, ds)| if ds.is_empty() { Some(n.clone()) } else { None })
        .collect();

    let mut ordered: Vec<TableSchema> = Vec::with_capacity(by_name.len());
    while let Some(next) = ready.iter().next().cloned() {
        ready.remove(&next);
        // Move the table out of by_name into the ordered list.
        let t = by_name
            .remove(&next)
            .expect("ready entries are always still in by_name");
        ordered.push(t);
        // Decrement in-degree of every dependent of `next`. The
        // dependent's deps set holds `next`; remove it. If the
        // dependent's deps set becomes empty, queue it.
        let dependents: Vec<String> = reverse
            .get(&next)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        for dependent in dependents {
            let Some(d) = deps.get_mut(&dependent) else {
                continue;
            };
            d.remove(&next);
            if d.is_empty() {
                ready.insert(dependent);
            }
        }
    }

    // Acyclic case: by_name is empty, no follow-up FKs needed.
    if by_name.is_empty() {
        return (ordered, Vec::new());
    }

    // Cycle case: every table left in by_name participates in (or is
    // downstream of) a cycle. Strip their inline FKs that point at
    // any other table still in by_name, emit them in alphabetical
    // order, and synthesise standalone AddForeignKey follow-ups.
    let cycle_members: BTreeSet<String> = by_name.keys().cloned().collect();
    let mut follow_up_fks: Vec<SchemaOperation> = Vec::new();

    // Drain by_name in alphabetical order (BTreeMap iteration is
    // sorted). Mutate each table to strip cycle-internal inline FKs
    // and capture the stripped FKs as follow-up AddForeignKey ops.
    for (name, mut t) in std::mem::take(&mut by_name) {
        let owning_table = t.table.clone();
        for col in t.columns.iter_mut() {
            let strip = col
                .foreign_key
                .as_ref()
                .map(|fk| cycle_members.contains(&fk.ref_table))
                .unwrap_or(false);
            if strip {
                let fk = col
                    .foreign_key
                    .take()
                    .expect("strip implies foreign_key.is_some()");
                // Mirror the column's relation_kind / on_delete back
                // out — they no longer apply to the inlined column,
                // but the FK schema carries on_delete on its own
                // field for the standalone constraint emission.
                follow_up_fks.push(SchemaOperation::AddForeignKey {
                    table: owning_table.clone(),
                    column: col.name.clone(),
                    fk,
                });
            }
        }
        ordered.push(t);
        // Suppress an unused-variable warning if the loop body never
        // touches `name` directly (it's the BTreeMap key we already
        // consumed via `t.table`).
        let _ = name;
    }

    (ordered, follow_up_fks)
}

fn operation_phase(op: &SchemaOperation) -> usize {
    match op {
        SchemaOperation::AddEnum(_) => 0,
        // RenameTable runs BEFORE AddTable so `CREATE TABLE` payloads
        // that inline `REFERENCES <new_name>` resolve cleanly. See
        // `order_operations` for the full rationale.
        SchemaOperation::RenameTable { .. } => 1,
        SchemaOperation::AddTable(_) => 2,
        SchemaOperation::AddColumn { .. }
        | SchemaOperation::RenameColumn { .. }
        | SchemaOperation::AlterColumn { .. }
        | SchemaOperation::AddForeignKey { .. }
        | SchemaOperation::SetTableComment { .. }
        | SchemaOperation::SetStorageParams { .. }
        | SchemaOperation::SetTablespace { .. } => 3,
        SchemaOperation::AddEnumVariant { .. } => 4,
        SchemaOperation::AddIndex(_) => 5,
        // EXCLUDE constraints sit alongside indexes in the phase
        // ordering — they share index-method semantics (GIST / BTREE)
        // and depend on the columns being present, so they run after
        // AddTable / AddColumn but before any drops. The
        // AddExclusionConstraint variant is OfflineOnly per the v3
        // plan, so it never reaches the live runner; this phase
        // value is for the synchronous `compose` ordering only.
        SchemaOperation::AddExclusionConstraint { .. } => 5,
        SchemaOperation::DropIndex(_) => 6,
        SchemaOperation::DropExclusionConstraint { .. } => 6,
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
        SchemaOperation::PkTypeFlip { .. }
        | SchemaOperation::PkTypeFlipGroup(_)
        | SchemaOperation::PkTypeFlipMultiGroup(_)
        | SchemaOperation::Unsupported { .. } => 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::diff::{
        ColumnChange, PkFlipJoinTableOption, PkFlipPartitionedMeta, PkTypeFlipGroup, SchemaDelta,
    };
    use crate::migrate::projection::BucketKey;
    use crate::migrate::schema::{
        ColumnSchema, EnumSchema, ForeignKeySchema, IndexColumnSchema, IndexKindSchema,
        IndexNullsOrderSchema, IndexOrderSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
        OnDeleteSchema, PkKindSchema, PrimaryKeySchema, TableSchema,
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
            comment: None,
            default_sql: None,
            foreign_key: None,
            generated: None,
            identity: None,
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
            type_change_using: None,
        }
    }

    fn id_column_heerid() -> ColumnSchema {
        ColumnSchema {
            default_sql: Some("heerid_next()".to_string()),
            ..col("id", "BIGINT", false)
        }
    }

    fn numeric_array_column(name: &str) -> ColumnSchema {
        ColumnSchema {
            check: Some("djogi.__djogi_numeric_array_is_rust_decimal_v1(\"amounts\")".to_string()),
            ..col(name, "NUMERIC[]", true)
        }
    }

    fn date_array_column(name: &str) -> ColumnSchema {
        ColumnSchema {
            check: Some(format!("djogi.__djogi_date_array_is_finite_v1(\"{name}\")")),
            ..col(name, "DATE[]", true)
        }
    }

    fn tstz_array_column(name: &str) -> ColumnSchema {
        ColumnSchema {
            check: Some(format!("djogi.__djogi_tstz_array_is_finite_v1(\"{name}\")")),
            ..col(name, "TIMESTAMPTZ[]", true)
        }
    }

    fn synth_table(name: &str) -> TableSchema {
        TableSchema {
            app: None,
            columns: vec![id_column_heerid(), col("name", "TEXT", true)],
            exclusion_constraints: Vec::new(),
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
            table_comment: None,
            storage_params: None,
            tablespace: None,
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

    #[test]
    fn numeric_array_check_triggers_helper_preload_in_plan() {
        let mut table = synth_table("widgets");
        table.columns.push(numeric_array_column("amounts"));
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::AddTable(table)],
            classification: Classification::Additive,
        };

        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
        let labels: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        assert!(
            labels.len() >= 2,
            "expected helper + table statements; labels: {labels:?}"
        );
        assert_eq!(labels[0], "Ensure djogi numeric-array helper");
        assert!(
            plan.segments[0].statements[0]
                .up
                .contains("CREATE SCHEMA IF NOT EXISTS djogi;"),
            "helper SQL must be prepended so execution creates djogi function",
        );
        assert!(
            plan.segments[0].statements[1]
                .up
                .contains("CONSTRAINT \"widgets_amounts_check\""),
            "table DDL should keep the generated NUMERIC[] constraint"
        );
    }

    #[test]
    fn date_and_tstz_array_helpers_inject_before_table_in_compose_order() {
        // A table carrying both a DATE[] column and a TIMESTAMPTZ[] column
        // triggers both `requires_date_array_helper` and
        // `requires_tstz_array_helper`.  `plan_delta` must inject the two
        // helper operations BEFORE the table DDL and in the same order that
        // `compose_up_text` / `compose_down_text` emit them (date first,
        // then tstz).
        //
        // The prior `insert(0, …)` implementation reversed the order:
        // each insert pushes all previous entries down by one, so the
        // last-inserted helper (tstz) ended at index 0 and date ended at
        // index 1 — the opposite of the compose file order.
        let mut table = synth_table("events");
        table.columns.push(date_array_column("blackout_dates"));
        table.columns.push(tstz_array_column("scheduled_slots"));
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::AddTable(table)],
            classification: Classification::Additive,
        };

        let plan = plan_delta(&delta).expect("ok");
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
        let labels: Vec<&str> = plan.segments[0]
            .statements
            .iter()
            .map(|s| s.label.as_str())
            .collect();

        assert!(
            labels.len() >= 3,
            "expected date helper + tstz helper + table DDL; labels: {labels:?}"
        );

        let date_pos = labels
            .iter()
            .position(|l| *l == "Ensure djogi date-array finite-element helper")
            .expect("date-array helper not found in plan labels");
        let tstz_pos = labels
            .iter()
            .position(|l| *l == "Ensure djogi timestamptz-array finite-element helper")
            .expect("tstz-array helper not found in plan labels");
        let table_pos = labels
            .iter()
            .position(|l| l.starts_with("AddTable"))
            .expect("AddTable statement not found in plan labels");

        assert!(
            date_pos < table_pos,
            "date-array helper must precede table DDL (compose order); labels: {labels:?}"
        );
        assert!(
            tstz_pos < table_pos,
            "tstz-array helper must precede table DDL (compose order); labels: {labels:?}"
        );
        assert!(
            date_pos < tstz_pos,
            "date-array helper must precede tstz-array helper \
             (matches compose_up_text prelude order); labels: {labels:?}"
        );
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
                    fk: ForeignKeySchema {
                        deferrable: false,
                        initially_deferred: false,
                        on_delete: OnDeleteSchema::Restrict,
                        ref_column: "id".to_string(),
                        ref_table: "users".to_string(),
                    },
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

    #[test]
    fn partitioned_multi_parent_cluster_errors_during_planning() {
        let mut left = PkTypeFlipGroup {
            parent_table: "left_events".to_string(),
            parent_from: PkKindSchema::HeerId,
            parent_to: PkKindSchema::HeerIdRecencyBiased,
            direction: crate::migrate::diff::PkFlipDirection::AscToDesc,
            children: Vec::new(),
            self_fk: None,
            join_tables: Vec::new(),
            cycles: Vec::new(),
            partitioned_parent: Some(PkFlipPartitionedMeta {
                partition: crate::migrate::schema::PartitionSchema::Range {
                    column: "ts".to_string(),
                },
            }),
            co_destructive: false,
            co_lossy: false,
            join_table_option: PkFlipJoinTableOption::OptionA,
        };
        let right = PkTypeFlipGroup {
            parent_table: "right_tags".to_string(),
            parent_from: PkKindSchema::HeerId,
            parent_to: PkKindSchema::HeerIdRecencyBiased,
            direction: crate::migrate::diff::PkFlipDirection::AscToDesc,
            children: Vec::new(),
            self_fk: None,
            join_tables: Vec::new(),
            cycles: Vec::new(),
            partitioned_parent: None,
            co_destructive: false,
            co_lossy: false,
            join_table_option: PkFlipJoinTableOption::OptionA,
        };
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::PkTypeFlipMultiGroup(vec![
                left.clone(),
                right.clone(),
            ])],
            classification: Classification::PkTypeFlip {
                co_destructive: false,
                co_lossy: false,
            },
        };
        let err = plan_delta(&delta).expect_err("must reject partitioned multi-parent cluster");
        match err {
            SqlEmitError::Diff(
                crate::migrate::diff::DiffError::PartitionedMultiParentClusterUnsupported {
                    partitioned_parents,
                    cross_flipping_partners,
                },
            ) => {
                assert_eq!(partitioned_parents, vec!["left_events".to_string()]);
                assert_eq!(
                    cross_flipping_partners,
                    vec!["left_events".to_string(), "right_tags".to_string()]
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }

        left.partitioned_parent = None;
        let ok_delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::PkTypeFlipMultiGroup(vec![left, right])],
            classification: Classification::PkTypeFlip {
                co_destructive: false,
                co_lossy: false,
            },
        };
        let plan = plan_delta(&ok_delta).expect("non-partitioned multi-group should still lower");
        assert!(!plan.segments.is_empty());
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

    // ── AddTable toposort ───────────────────────────────────────────

    /// Build a column carrying an inline FK pointing at `target` with
    /// the project-default `Restrict` cascade.
    fn fk_col(name: &str, target: &str) -> ColumnSchema {
        ColumnSchema {
            check: None,
            comment: None,
            default_sql: None,
            foreign_key: Some(ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: target.to_string(),
            }),
            generated: None,
            identity: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: name.to_string(),
            nullable: false,
            on_delete: Some(OnDeleteSchema::Restrict),
            outbox_exclude: false,
            rationale: None,
            relation_kind: Some(crate::migrate::schema::RelationKindSchema::ForeignKey),
            renamed_from: None,
            sequence_within: None,
            sql_type: "BIGINT".to_string(),
            unique: false,
            type_change_using: None,
        }
    }

    fn table_with_fk(name: &str, fk_to: &[(&str, &str)]) -> TableSchema {
        let mut cols = vec![id_column_heerid()];
        for (col_name, target) in fk_to {
            cols.push(fk_col(col_name, target));
        }
        TableSchema {
            app: None,
            columns: cols,
            exclusion_constraints: Vec::new(),
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
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    /// Position of the first `AddTable` for `name` in a plan's
    /// flattened statement labels. Helper to keep the toposort tests
    /// readable.
    fn add_table_pos(plan: &MigrationPlan, name: &str) -> Option<usize> {
        plan.segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .position(|s| s.label == format!("AddTable {name}"))
    }

    #[test]
    fn fk_chain_two_tables_orders_target_before_dependent() {
        // `accounts` (depends on `users`) must be created AFTER
        // `users`. The differ feeds `AddTable` ops via BTreeMap so
        // alphabetical input order is `accounts`, `users` — without
        // toposort we'd hit "relation users does not exist" at
        // apply time.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(table_with_fk("accounts", &[("user_id", "users")])),
                SchemaOperation::AddTable(synth_table("users")),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("plan");
        let users_pos = add_table_pos(&plan, "users").expect("users emitted");
        let accounts_pos = add_table_pos(&plan, "accounts").expect("accounts emitted");
        assert!(
            users_pos < accounts_pos,
            "users must precede accounts; users={users_pos}, accounts={accounts_pos}"
        );
    }

    #[test]
    fn fk_chain_three_tables_orders_deepest_target_first() {
        // C → B → A. Expect A, B, C.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(table_with_fk("c", &[("b_id", "b")])),
                SchemaOperation::AddTable(table_with_fk("b", &[("a_id", "a")])),
                SchemaOperation::AddTable(synth_table("a")),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("plan");
        let a = add_table_pos(&plan, "a").expect("a");
        let b = add_table_pos(&plan, "b").expect("b");
        let c = add_table_pos(&plan, "c").expect("c");
        assert!(
            a < b && b < c,
            "expected a < b < c; got a={a}, b={b}, c={c}"
        );
    }

    #[test]
    fn fk_cycle_breaks_inline_fks_and_emits_follow_up_add_foreign_key() {
        // `a` has FK to `b`; `b` has FK to `a`. Tables emit without
        // their inline FKs (alphabetical: a, b). Two `AddForeignKey`
        // ops follow.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(table_with_fk("a", &[("b_id", "b")])),
                SchemaOperation::AddTable(table_with_fk("b", &[("a_id", "a")])),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("plan");
        let labels: Vec<_> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();

        let a_pos = labels
            .iter()
            .position(|l| *l == "AddTable a")
            .expect("AddTable a");
        let b_pos = labels
            .iter()
            .position(|l| *l == "AddTable b")
            .expect("AddTable b");
        assert!(a_pos < b_pos, "alphabetical: a before b");

        let fk_a_pos = labels
            .iter()
            .position(|l| *l == "AddForeignKey a.b_id")
            .expect("AddForeignKey a.b_id");
        let fk_b_pos = labels
            .iter()
            .position(|l| *l == "AddForeignKey b.a_id")
            .expect("AddForeignKey b.a_id");
        assert!(b_pos < fk_a_pos, "AddTable b before AddForeignKey a.b_id");
        assert!(b_pos < fk_b_pos, "AddTable b before AddForeignKey b.a_id");

        // Sanity: the `CREATE TABLE a` statement should not contain
        // `REFERENCES "b"` because the FK was stripped out.
        let create_a = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .find(|s| s.label == "AddTable a")
            .expect("AddTable a statement");
        assert!(
            !create_a.up.contains("REFERENCES \"b\""),
            "cycle-breaking must strip inline FK; got: {}",
            create_a.up
        );
    }

    #[test]
    fn independent_tables_keep_alphabetical_order_for_determinism() {
        // No FKs at all — toposort starts with all three tables in
        // the ready set and pops alphabetically. Output order is
        // `a`, `b`, `c`.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                SchemaOperation::AddTable(synth_table("c")),
                SchemaOperation::AddTable(synth_table("a")),
                SchemaOperation::AddTable(synth_table("b")),
            ],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("plan");
        let a = add_table_pos(&plan, "a").expect("a");
        let b = add_table_pos(&plan, "b").expect("b");
        let c = add_table_pos(&plan, "c").expect("c");
        assert!(
            a < b && b < c,
            "alphabetical determinism: a < b < c; got a={a}, b={b}, c={c}"
        );
    }

    #[test]
    fn self_referencing_table_does_not_trigger_cycle_break() {
        // A table whose FK points at itself is fine in Postgres
        // (the inline `REFERENCES same_table` succeeds because the
        // table exists by the time the constraint check runs). The
        // toposort drops self-edges so the table emits with its
        // inline FK intact — no follow-up AddForeignKey op.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::AddTable(table_with_fk(
                "tree",
                &[("parent_id", "tree")],
            ))],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("plan");
        let stmts: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(stmts, vec!["AddTable tree"]);
        // The CREATE TABLE statement should still carry the inline
        // REFERENCES clause for the self-FK.
        let stmt = &plan.segments[0].statements[0];
        assert!(
            stmt.up.contains("REFERENCES \"tree\""),
            "self-FK must remain inline; got: {}",
            stmt.up
        );
    }

    #[test]
    fn external_fk_target_does_not_constrain_batch_ordering() {
        // `widgets` references `external_users` which is NOT in this
        // batch (it already exists in the live schema). The FK is
        // emitted inline; the toposort drops the out-of-batch edge
        // so no false dependency forms.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![SchemaOperation::AddTable(table_with_fk(
                "widgets",
                &[("owner_id", "external_users")],
            ))],
            classification: Classification::Additive,
        };
        let plan = plan_delta(&delta).expect("plan");
        // Single AddTable, no follow-ups, no cycle break.
        let labels: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(labels, vec!["AddTable widgets"]);
    }

    // ── RenameTable hoisted ahead of AddTable ───────────────────────

    #[test]
    fn rename_table_runs_before_add_table_referencing_post_rename_name() {
        // When `RenameTable users → members` pairs with `AddTable comments`
        // where `comments.user_id`
        // points at the post-rename name `"members"`. With RenameTable
        // hoisted to phase 1 (ahead of AddTable in phase 2), the
        // rename runs first; the inline `REFERENCES "members"` then
        // resolves at apply time.
        //
        // The toposort still treats `"members"` as out-of-batch (it
        // is not in the AddTable set), so the FK is inlined in the
        // CREATE TABLE — exactly the path the bug originally broke.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                // Note: AddTable listed BEFORE RenameTable in input
                // order, to confirm the phase ordering — not the
                // input order — drives the result.
                SchemaOperation::AddTable(table_with_fk("comments", &[("user_id", "members")])),
                SchemaOperation::RenameTable {
                    from: "users".to_string(),
                    to: "members".to_string(),
                },
            ],
            classification: Classification::Reversible,
        };
        let plan = plan_delta(&delta).expect("plan");
        let labels: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        let rename_pos = labels
            .iter()
            .position(|l| l.starts_with("RenameTable"))
            .expect("RenameTable in plan");
        let add_pos = labels
            .iter()
            .position(|l| *l == "AddTable comments")
            .expect("AddTable comments in plan");
        assert!(
            rename_pos < add_pos,
            "RenameTable must precede AddTable; labels: {labels:?}"
        );
        // Confirm the inline FK survived in the CREATE TABLE.
        let create_stmt = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .find(|s| s.label == "AddTable comments")
            .expect("AddTable comments stmt");
        assert!(
            create_stmt.up.contains("REFERENCES \"members\" (\"id\")"),
            "inline FK must point at post-rename target; got: {}",
            create_stmt.up
        );
        // SQL-string-level proof: the apply-time `up` stream must show
        // `ALTER TABLE ... RENAME TO "members"` strictly before
        // `CREATE TABLE "comments"`. Operation-label ordering above is
        // necessary but not sufficient — Postgres sees the SQL stream,
        // not the label list. Concatenate every segment's `up` text in
        // emit order and assert byte positions.
        let up_stream: String = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.up.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let rename_sql_pos = up_stream
            .find("RENAME TO \"members\"")
            .expect("RENAME TO members in up stream");
        let create_sql_pos = up_stream
            .find("CREATE TABLE \"comments\"")
            .expect("CREATE TABLE comments in up stream");
        assert!(
            rename_sql_pos < create_sql_pos,
            "ALTER TABLE ... RENAME TO must precede CREATE TABLE in up stream; \
             rename_sql_pos={rename_sql_pos}, create_sql_pos={create_sql_pos}, \
             stream:\n{up_stream}"
        );
    }

    #[test]
    fn rename_table_runs_before_add_table_alphabetical_determinism() {
        // Multiple RenameTables + AddTables in one delta. Within each
        // phase the stable sort preserves input order (the differ
        // already feeds alphabetical input via BTreeMap iteration), so
        // emit order is alphabetical within renames, then alphabetical
        // within adds — and renames as a whole precede adds as a
        // whole.
        let delta = SchemaDelta {
            bucket: bucket(),
            operations: vec![
                // Input alphabetical, mixed across phases.
                SchemaOperation::RenameTable {
                    from: "alpha_old".to_string(),
                    to: "alpha".to_string(),
                },
                SchemaOperation::AddTable(synth_table("apples")),
                SchemaOperation::RenameTable {
                    from: "beta_old".to_string(),
                    to: "beta".to_string(),
                },
                SchemaOperation::AddTable(synth_table("bananas")),
            ],
            classification: Classification::Reversible,
        };
        let plan = plan_delta(&delta).expect("plan");
        let labels: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.label.as_str())
            .collect();
        // Expected: RenameTable alpha_old → alpha, RenameTable
        // beta_old → beta, AddTable apples, AddTable bananas. The
        // helper formats RenameTable labels via lower_operation; we
        // assert the relative order rather than the exact label
        // strings to keep this test resilient to label tweaks.
        let rename_alpha = labels
            .iter()
            .position(|l| l.contains("alpha_old"))
            .expect("rename alpha");
        let rename_beta = labels
            .iter()
            .position(|l| l.contains("beta_old"))
            .expect("rename beta");
        let add_apples = labels
            .iter()
            .position(|l| *l == "AddTable apples")
            .expect("add apples");
        let add_bananas = labels
            .iter()
            .position(|l| *l == "AddTable bananas")
            .expect("add bananas");
        assert!(
            rename_alpha < rename_beta,
            "renames keep alphabetical order; labels: {labels:?}"
        );
        assert!(
            rename_beta < add_apples,
            "all renames precede all adds; labels: {labels:?}"
        );
        assert!(
            add_apples < add_bananas,
            "adds keep alphabetical order; labels: {labels:?}"
        );
        // SQL-string-level proof: same as the single-pair test, but
        // sweeping the multi-rename / multi-add case. Both
        // `RENAME TO "alpha"` and `RENAME TO "beta"` must precede
        // `CREATE TABLE "apples"` and `CREATE TABLE "bananas"` in the
        // concatenated up stream.
        let up_stream: String = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter())
            .map(|s| s.up.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let rename_alpha_sql = up_stream
            .find("RENAME TO \"alpha\"")
            .expect("RENAME TO alpha in up stream");
        let rename_beta_sql = up_stream
            .find("RENAME TO \"beta\"")
            .expect("RENAME TO beta in up stream");
        let create_apples_sql = up_stream
            .find("CREATE TABLE \"apples\"")
            .expect("CREATE TABLE apples in up stream");
        let create_bananas_sql = up_stream
            .find("CREATE TABLE \"bananas\"")
            .expect("CREATE TABLE bananas in up stream");
        assert!(
            rename_alpha_sql < rename_beta_sql,
            "RENAME alpha must precede RENAME beta in up stream"
        );
        assert!(
            rename_beta_sql < create_apples_sql,
            "all RENAME TO must precede all CREATE TABLE in up stream"
        );
        assert!(
            create_apples_sql < create_bananas_sql,
            "CREATE apples must precede CREATE bananas in up stream"
        );
    }
}
