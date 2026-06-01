//! PK-type-flip migration SQL emission and segment planning — T9 of
//! the Phase 7 v3 plan.
//!
//! # What this module owns
//!
//! Lowering of [`SchemaOperation::PkTypeFlipGroup`] into the
//! multi-segment [`MigrationPlan`] required by the HeeRanjID
//! `asc-to-desc` playbook. Each emitted SQL statement matches the
//! playbook's worked-example shape semantically; the playbook is
//! prose with worked-example identifiers (`tbl` / `id` / `id_desc`)
//! and this module emits SQL parameterised by per-group state, so
//! the two are NOT byte-identical. The regression net is split
//! into two layers:
//!
//!   * **Emitter-output drift detector** — fixtures under
//!     `fixtures/pk_flip_emitter_output_section_*.sql` capture the
//!     CURRENT emitter's whitespace-normalised output for the
//!     canonical worked examples. Tests under
//!     `tests::whole_plan_byte_equality_section_*` and
//!     `tests::emitter_output_drift_check_section_*` assert the
//!     emitter's output equals these fixtures byte-for-byte after
//!     normalisation. ANY emitter change without a paired fixture
//!     update fails loud.
//!   * **Playbook structural anchors** — tests under
//!     `tests::fixture_section_*_carries_every_playbook_anchor_substring`
//!     walk each fixture and assert the presence of every
//!     load-bearing playbook substring (specific
//!     `CALL heeranjid_bulk_backfill(...)` / `ALTER TABLE ... SET
//!     NOT NULL` / `CREATE UNIQUE INDEX CONCURRENTLY` shapes). If
//!     the playbook adds or removes a step, that test must be
//!     updated. The two-sided invariant catches both emitter
//!     drift AND playbook drift.
//!
//! The playbook lives at
//! `../HeeRanjID/docs/migrations/asc-to-desc.md`. Where this module
//! and the playbook disagree on a load-bearing rule, the playbook
//! wins.
//!
//! # Plan shape (single-table flip — playbook §3)
//!
//! | Segment | Kind            | Statements                                                              |
//! |---------|-----------------|-------------------------------------------------------------------------|
//! |   1     | Transactional   | `ALTER TABLE … ADD COLUMN id_desc bigint;` + autofill trigger install   |
//! |   2     | NonTransactional| `CALL heeranjid_bulk_backfill(...)` (per child) + verification SELECT   |
//! |   3     | NonTransactional| `CREATE UNIQUE INDEX CONCURRENTLY idx_<tbl>_id_desc ON <tbl> (id_desc);`|
//! |   4     | Transactional   | NOT NULL proof: `CHECK (... NOT NULL) NOT VALID; VALIDATE; SET NOT NULL`|
//! |   5     | Transactional   | Cutover: drop old PK, promote new index, drop trigger, rename column    |
//!
//! Parent + child / multi-level / self-FK / join / cycle /
//! partitioned variations extend this base shape per playbook §4 / §6
//! / §7 / §8 / §9. **The cutover (segment 5) is always one atomic
//! Postgres transaction across parent + every child** — that is the
//! atomicity invariant the playbook calls out as load-bearing.
//!
//! # Reverse direction (Desc → Asc)
//!
//! Reverse migrations mirror the forward shape and substitute every
//! occurrence of `_desc` shadow naming with `_asc`, every flip-fn
//! invocation (`heerid_to_desc` / `ranjid_to_desc`) with its
//! symmetric (`heerid_to_asc` / `ranjid_to_asc`), and every
//! generator default (`heerid_next_desc()` / `ranjid_next_desc()`)
//! with the ascending variant (`heerid_next()` / `ranjid_next()`).
//! The structural transactions and segment ordering remain identical
//! so the reverse path is reviewable side-by-side with the forward
//! path. We document the mirroring decision here in plain English
//! rather than via any pattern-matching shorthand — this codebase
//! contains no regex.
//!
//! # Rollback boundary (point of no return)
//!
//! The cutover transaction (segment 5) is the **point of no return**.
//! Once it commits the old `id` column, its DEFAULT, and the autofill
//! trigger are gone; rollback requires a fresh inverse migration
//! (add the asc column back, install reverse trigger, re-backfill,
//! cutover again). We mark segment 5's first OperationSql with a
//! [`LossyRollbackKind::PkTypeFlipPostCutover`] warning so the
//! runner / `migrations status` surface the boundary loudly.
//!
//! Segments 1 — 4 carry a clean inverse (drop the shadow column,
//! drop the trigger, drop the CHECK constraint, drop the unique
//! index); their `down` SQL reverses cleanly without data loss.
//!
//! # Determinism
//!
//! The lowered SQL is byte-stable across runs given the same
//! [`PkTypeFlipGroup`] input. Sub-collections inside the group
//! (children, self-FK columns, join tables, cycles) are pre-sorted
//! by the differ before reaching this module so the emitter walks
//! them in deterministic order without re-sorting.

use std::fmt::Write as _;

use super::diff::{PkFlipChild, PkFlipDirection, PkFlipFamily, PkTypeFlipGroup};
use super::projection::BucketKey;
use super::schema::{OnDeleteSchema, PartitionSchema};
use super::segment::{MigrationPlan, Segment, SegmentKind};
use super::sql::{LossyRollbackKind, LossyRollbackWarning, OperationSql};

// ── Public façade ────────────────────────────────────────────────────────

/// Public-input validation failures for [`lower_pk_flip_group`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkFlipError {
    /// The self-FK sidecar vectors must stay index-aligned. A mismatch
    /// means the caller handed `lower_pk_flip_group` a malformed group
    /// that cannot preserve per-FK deferrability deterministically.
    MalformedSelfFkMetadata {
        parent_table: String,
        fk_columns: usize,
        fk_constraint_names: usize,
        fk_deferrable: usize,
        fk_initially_deferred: usize,
    },
}

impl std::fmt::Display for PkFlipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PkFlipError::MalformedSelfFkMetadata {
                parent_table,
                fk_columns,
                fk_constraint_names,
                fk_deferrable,
                fk_initially_deferred,
            } => write!(
                f,
                "PK-flip group for `{parent_table}` has mismatched self-FK metadata lengths: \
                 fk_columns={fk_columns}, fk_constraint_names={fk_constraint_names}, \
                 fk_deferrable={fk_deferrable}, \
                 fk_initially_deferred={fk_initially_deferred}"
            ),
        }
    }
}

impl std::error::Error for PkFlipError {}

/// Lower a [`PkTypeFlipGroup`] into the multi-segment migration plan
/// per the HeeRanjID playbook.
///
/// **Whole-migration kind.** The plan returned is always
/// non-transactional overall (per Phase 7-Zero v3 §6.2 deterministic
/// A): segment classifications alternate Transactional / NonTransactional
/// as the playbook requires, but the migration as a whole is recorded
/// as `non_transactional` in the ledger because at least one segment
/// runs outside a Postgres transaction (the backfill `CALL` and the
/// `CREATE INDEX CONCURRENTLY`).
///
/// **Bucket** — the caller supplies the bucket; the emitter writes
/// it onto the resulting plan verbatim.
///
/// **Determinism** — the SQL is byte-stable across runs. See the
/// module doc for sub-collection ordering rules.
pub fn lower_pk_flip_group(
    group: &PkTypeFlipGroup,
    bucket: BucketKey,
) -> Result<MigrationPlan, PkFlipError> {
    let segments = build_segments(group)?;
    Ok(MigrationPlan {
        bucket,
        classification: super::diff::Classification::PkTypeFlip {
            co_destructive: group.co_destructive,
            co_lossy: group.co_lossy,
        },
        segments,
    })
}

fn validate_group(group: &PkTypeFlipGroup) -> Result<(), PkFlipError> {
    if let Some(self_fk) = &group.self_fk {
        let expected = self_fk.fk_columns.len();
        let actuals = [
            self_fk.fk_constraint_names.len(),
            self_fk.fk_deferrable.len(),
            self_fk.fk_initially_deferred.len(),
        ];
        if actuals.iter().any(|actual| *actual != expected) {
            return Err(PkFlipError::MalformedSelfFkMetadata {
                parent_table: group.parent_table.clone(),
                fk_columns: expected,
                fk_constraint_names: self_fk.fk_constraint_names.len(),
                fk_deferrable: self_fk.fk_deferrable.len(),
                fk_initially_deferred: self_fk.fk_initially_deferred.len(),
            });
        }
    }
    Ok(())
}

/// Build the segment list for a single [`PkTypeFlipGroup`].
///
/// Public-in-crate so the segment planner can splice this directly
/// into a multi-bucket plan when the caller is composing a delta
/// that mixes a flip with non-flip ops in OTHER buckets (the same
/// bucket cannot mix both — the differ enforces that invariant via
/// the per-bucket op list).
pub(crate) fn build_segments(group: &PkTypeFlipGroup) -> Result<Vec<Segment>, PkFlipError> {
    validate_group(group)?;
    if let Some(part) = &group.partitioned_parent {
        return Ok(build_segments_partitioned(group, part));
    }
    let mut segments: Vec<Segment> = Vec::new();

    // Segment 1 — preparation (transactional). One transaction
    // installs the parent's shadow column + autofill trigger plus
    // every child's shadow column and trigger. Child NOT-VALID FK
    // statements pointing at `parent(id_desc)` cannot land here —
    // Postgres requires the target column to carry a unique
    // constraint at constraint-creation time, even with NOT VALID.
    // Those FK statements are emitted in segment 3b after the
    // concurrent UNIQUE INDEX on `parent.id_desc` lands.
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![emit_preparation(group)],
    });

    // Segment 2 — backfill (non-transactional). Each backfill is its
    // own `CALL`; the procedure manages internal commits per batch.
    // Emit ONE OperationSql per CALL / VALIDATE statement so the
    // runner dispatches each through the internal single-statement
    // batch path; the procedure's internal `COMMIT`s would otherwise raise
    // `invalid transaction termination` when wrapped in the implicit
    // simple-query batch transaction.
    segments.push(Segment {
        kind: SegmentKind::NonTransactional,
        statements: emit_backfill_statements(group),
    });

    // Segment 2b — verification halt point (transactional in classification
    // but the runner intercepts each `PkFlipVerify` statement and runs it
    // as a count-assert against the live DB; halts on non-zero count with
    // `RunnerError::PkFlipVerificationFailed`).
    let verifications = emit_verification_statements(group);
    if !verifications.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::Transactional,
            statements: verifications,
        });
    }

    // Segment 3 — concurrent unique-index build (non-transactional).
    // CONCURRENTLY MUST run outside any transaction; one statement
    // per OperationSql so the simple-query batch never wraps them.
    segments.push(Segment {
        kind: SegmentKind::NonTransactional,
        statements: emit_concurrent_index_statements(group),
    });

    // Segment 3b — child / self-FK / join-table NOT-VALID FK
    // creation (transactional). Postgres requires the target column
    // to carry a unique constraint at FK-creation time even with
    // NOT VALID — the parent's `id_desc` only has that constraint
    // after segment 3's CREATE UNIQUE INDEX CONCURRENTLY commits, so
    // the FK statements wait until here.
    let fk_stmts = emit_child_fk_statements(group);
    if !fk_stmts.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::Transactional,
            statements: fk_stmts,
        });
    }

    // Segment 4 — NOT NULL proof (transactional).
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![emit_not_null_proof(group)],
    });

    // Segment 5 — cutover (transactional, single atomic tx covering
    // parent + every child + every join table + every cycle peer).
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![emit_cutover(group)],
    });

    Ok(segments)
}

/// Build the segment list for a [`SchemaOperation::PkTypeFlipMultiGroup`].
///
/// **Stage interleaving — playbook §7 line 327.** Where
/// [`build_segments`] produces a 5-segment plan for ONE parent, this
/// fn produces a 5-segment plan for the WHOLE cluster — at each
/// stage, every member group's stage-N statements are emitted in
/// alphabetical-by-parent order. This is the only structurally
/// correct lowering for a cross-flipping cluster: stage 3b needs
/// every parent's `id_desc` shadow column to exist (created at
/// stage 1), so parent A's stage 3b cannot run BEFORE parent B's
/// stage 1 in the back-to-back layout. With stage interleaving,
/// every parent's stage 1 runs first, then every parent's stage 2,
/// etc. — by the time stage 3b emits `... FOREIGN KEY (book_id_desc)
/// REFERENCES jt_books(id_desc)` AND `... FOREIGN KEY (tag_id_desc)
/// REFERENCES jt_tags(id_desc)`, both `id_desc` columns exist.
///
/// **Cutover atomicity.** All groups' cutover statements concatenate
/// into ONE [`OperationSql`] in ONE transactional segment — the
/// runner wraps the whole concatenated body in a single Postgres
/// transaction. This is the playbook §7 "single mega-transaction"
/// shape: drop every old FK, promote every parent's PK, finalise
/// every join table, all atomically.
///
/// **Empty-cluster guard.** Defensive: a zero-member multi-group
/// is structurally impossible (the merger only emits MultiGroup
/// when the cluster has 2+ members) but we still return an empty
/// segment vec rather than panicking — failing loudly on this
/// unreachable path would obscure real bugs in the merger.
///
/// **Partitioned member.** Currently the multi-parent merger does
/// not consider partitioned parents — playbook §9 single-parent
/// partitioned flips remain on the back-to-back path. A cluster
/// containing a partitioned parent is structurally rare (M:N
/// junctions to partitioned tables are unusual) and would require
/// extending the §9 emitters to interleave with non-partitioned
/// peers. Tracked as future work; the current merger code only
/// builds multi-groups from non-partitioned parents (the
/// partitioned-parent path triggers off `partitioned_parent =
/// Some(_)` and routes to `build_segments_partitioned` per group).
pub(crate) fn build_segments_multi(
    groups: &[PkTypeFlipGroup],
) -> Result<Vec<Segment>, super::diff::DiffError> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    // Validate every group's self-FK metadata before any lowering work.
    // Without this gate, malformed metadata would be silently truncated
    // by the inner emitters' `.get(i)` fallbacks. Single-member fallback
    // uses the same checked path
    // via `build_segments`, but multi-member clusters need explicit
    // validation here too because no member is dispatched through
    // `build_segments` (the multi lowerer composes statements
    // directly).
    for g in groups {
        validate_group(g)?;
    }
    if groups.len() == 1 {
        // A single-member cluster shouldn't reach here (the merger
        // only creates MultiGroup for 2+ members), but if it does,
        // fall back to the single-parent path so we don't waste a
        // segment on no-op interleaving.
        return Ok(build_segments(&groups[0])?);
    }
    if let Some(err) = super::diff::partitioned_multi_parent_cluster_error(groups) {
        return Err(err);
    }

    let mut segments: Vec<Segment> = Vec::new();

    // Stage 1 — preparation (transactional). Concatenate every
    // member's `emit_preparation` body into one OperationSql so
    // the runner's transactional segment wraps the whole
    // multi-parent prep in a single BEGIN/COMMIT pair.
    let mut prep_up = String::new();
    let mut prep_down = String::new();
    for g in groups {
        let prep = emit_preparation(g);
        if !prep_up.is_empty() && !prep.up.is_empty() {
            prep_up.push('\n');
        }
        prep_up.push_str(&prep.up);
        if !prep_down.is_empty() && !prep.down.is_empty() {
            // Down stack reverses: each member's down comes from
            // the END of `down` so the last-prepared parent rolls
            // back first. We push in member order here; the down
            // string is just a reversed concatenation — the
            // production runner does not currently auto-reverse
            // down ordering, so emitting them in member order
            // matches the segment classification's documentation
            // (segments 1..4 are reversibly-recoverable; segment
            // 5 is the point of no return).
            prep_down.push('\n');
        }
        prep_down.push_str(&prep.down);
    }
    let cluster_label = groups
        .iter()
        .map(|g| g.parent_table.as_str())
        .collect::<Vec<_>>()
        .join(",");
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![OperationSql {
            label: format!("PkFlipPrepMulti [{cluster_label}]"),
            up: prep_up,
            down: prep_down,
            lossy: None,
        }],
    });

    // Stage 2 — backfill (non-transactional). Each member's
    // backfill statements are emitted as their own OperationSql
    // entries; concatenate the per-member lists in alphabetical
    // order. The runner dispatches each statement through the internal
    // single-statement batch path; see the matching note on `build_segments`.
    let mut backfill_stmts: Vec<OperationSql> = Vec::new();
    for g in groups {
        backfill_stmts.extend(emit_backfill_statements(g));
    }
    segments.push(Segment {
        kind: SegmentKind::NonTransactional,
        statements: backfill_stmts,
    });

    // Stage 2b — verification halt point. Concatenate per-member
    // verifications; non-empty members contribute their statements
    // in alphabetical order.
    let mut verify_stmts: Vec<OperationSql> = Vec::new();
    for g in groups {
        verify_stmts.extend(emit_verification_statements(g));
    }
    if !verify_stmts.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::Transactional,
            statements: verify_stmts,
        });
    }

    // Stage 3 — concurrent unique-index build (non-transactional).
    // Each member's CONCURRENTLY indexes need their own
    // OperationSql. Order: every parent's parent-index first, then
    // every parent's child / join-table indexes — mirrors
    // `emit_concurrent_index_statements`'s per-group order, just
    // concatenated across members.
    let mut index_stmts: Vec<OperationSql> = Vec::new();
    for g in groups {
        index_stmts.extend(emit_concurrent_index_statements(g));
    }
    segments.push(Segment {
        kind: SegmentKind::NonTransactional,
        statements: index_stmts,
    });

    // Stage 3b — NOT VALID FK + VALIDATE pairs (transactional).
    // THIS is the stage that is correct only with interleaving:
    // by stage 3b every parent's `id_desc` exists (stage 1) AND has
    // its UNIQUE index (stage 3), so cross-flipping FKs that
    // reference partner shadows resolve. Concatenate every
    // member's FK statements in alphabetical order.
    let mut fk_stmts: Vec<OperationSql> = Vec::new();
    for g in groups {
        fk_stmts.extend(emit_child_fk_statements(g));
    }
    if !fk_stmts.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::Transactional,
            statements: fk_stmts,
        });
    }

    // Stage 4 — NOT NULL proof (transactional). Concatenate every
    // member's NOT NULL proof body into one OperationSql; the
    // runner wraps the whole thing in one transaction.
    let mut nn_up = String::new();
    let mut nn_down = String::new();
    for g in groups {
        let nn = emit_not_null_proof(g);
        if !nn_up.is_empty() && !nn.up.is_empty() {
            nn_up.push('\n');
        }
        nn_up.push_str(&nn.up);
        if !nn_down.is_empty() && !nn.down.is_empty() {
            nn_down.push('\n');
        }
        nn_down.push_str(&nn.down);
    }
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![OperationSql {
            label: format!("PkFlipNotNullProofMulti [{cluster_label}]"),
            up: nn_up,
            down: nn_down,
            lossy: None,
        }],
    });

    // Stage 5 — cutover (transactional, single atomic tx). The
    // multi-parent variant interleaves cutover phases ACROSS
    // members: drop ALL old FKs first, promote ALL parents
    // second, finalise ALL children third, finalise ALL join
    // tables last. This ordering is the structurally-correct one
    // for a cross-flipping cluster — the join-table finalisation
    // (phase 4) emits `ADD CONSTRAINT FOREIGN KEY (book_id)
    // REFERENCES jt_books(id)` AND `... REFERENCES jt_tags(id)`
    // in the same body, both of which only resolve correctly when
    // every member's phase 2 (parent promotion + RENAME `id_desc`
    // → `id`) has already run. Concatenating per-group cutover
    // bodies (phase 1..4 on parent A, then phase 1..4 on parent B)
    // would emit phase 4 on A before phase 2 on B, violating the
    // FK-target-exists constraint and tripping ri_triggers.c
    // post-COMMIT.
    let mut cut_up = String::new();
    // SET CONSTRAINTS ALL DEFERRED at top if ANY member has a
    // cycle. The cycle-deferral is the runner's signal that mid-
    // transaction FK states are tolerated until COMMIT. For a
    // multi-parent cluster with cycles on any member, the deferral
    // applies once at the cluster's transaction boundary.
    if groups.iter().any(|g| !g.cycles.is_empty()) {
        cut_up.push_str("SET CONSTRAINTS ALL DEFERRED;\n");
    }
    // Phase 1 across all members.
    for g in groups {
        cutover_phase_drop_old_fks(g, &mut cut_up);
    }
    // Phase 2 across all members. After this loop every parent's
    // `id_desc` has been renamed to `id`, so phase 4's
    // `REFERENCES <partner>(id)` resolves cleanly for every
    // partner targeting any cluster member.
    for g in groups {
        cutover_phase_promote_parent(g, &mut cut_up);
    }
    // Phase 3 across all members (each member's children
    // reference its own renamed `id`).
    for g in groups {
        cutover_phase_finalise_children(g, &mut cut_up);
    }
    // Phase 4 across all members. Only the winner has non-empty
    // `join_tables` after the merger; the loser's phase 4 no-ops.
    // This is where the structural fix shines: phase 4's `ADD
    // CONSTRAINT (... REFERENCES partner(id))` resolves because
    // every partner's phase 2 already ran.
    for g in groups {
        cutover_phase_finalise_join_tables(g, &mut cut_up);
    }
    let cut_down = format!(
        "-- POINT OF NO RETURN — segment 5 (cutover) for cluster [{cluster_label}] cannot be\n\
         -- reversed by `down` SQL alone. Rollback requires an inverse\n\
         -- migration: add the previous-direction columns back, install\n\
         -- reverse autofill triggers, re-run heeranjid_bulk_backfill on\n\
         -- every member, and run a second cutover. Plan that contingency\n\
         -- BEFORE running the forward cutover.",
    );
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![OperationSql {
            label: format!("PkFlipCutoverMulti [{cluster_label}]"),
            up: cut_up,
            down: cut_down,
            lossy: Some(LossyRollbackWarning {
                kind: LossyRollbackKind::PkTypeFlipPostCutover,
                detail: format!(
                    "POINT OF NO RETURN: cutover for cluster [{cluster_label}] removes \
                     prior PK columns and triggers across every member; rollback \
                     requires an inverse migration",
                ),
            }),
        }],
    });

    Ok(segments)
}

/// Build the segment list for a partitioned parent flip per
/// playbook §9 — composes the partitioned-parent specifics with the
/// cascade emitters used by [`build_segments`] so a partitioned
/// parent that ALSO has children, self-FK, join tables, or cycle
/// peers gets the full cutover orchestration in one plan.
///
/// **Composition (B-4).** The partitioned variant re-uses
/// `emit_preparation` (children/self-FK/join shadow columns +
/// triggers), `emit_backfill_statements` (per-table CALLs / DO
/// blocks for children/self-FK/join/cycles), `emit_verification_statements`
/// (per-table verification SELECTs marked PkFlipVerify), and
/// `emit_child_fk_statements` (NOT VALID FK + VALIDATE pairs), then
/// adds the partitioned-parent-specific extras for the parent table
/// itself (parent-level UNIQUE placeholder + per-leaf
/// CONCURRENTLY + ATTACH PARTITION expanded by the runner).
///
/// **Cutover.** The atomic cutover transaction touches the
/// partitioned parent (using `ADD PRIMARY KEY (partition_key,
/// id_desc)` because `USING INDEX` is illegal on a partitioned
/// parent) plus every cascade table.
fn build_segments_partitioned(
    group: &PkTypeFlipGroup,
    part: &super::diff::PkFlipPartitionedMeta,
) -> Vec<Segment> {
    // 1. Preparation: parent-level shadow column + multi-pair
    //    trigger via partitioned emitter, plus children/self-FK/join
    //    shadow columns + triggers via the cascade preparation
    //    emitter. The partitioned-parent shadow column propagates
    //    automatically to every leaf via the shared parent storage
    //    layout (PG13+).
    let mut prep_op = emit_partitioned_preparation(group, part);
    let cascade_prep = emit_preparation_children_only(group);
    if !cascade_prep.up.is_empty() {
        prep_op.up.push_str(&cascade_prep.up);
        prep_op.down = format!("{}\n{}", cascade_prep.down, prep_op.down);
    }

    // 2. Backfill: per-leaf CALLs for the parent (placeholder
    //    expanded by the runner) + per-table CALLs for every cascade
    //    member.
    let mut backfill_stmts: Vec<OperationSql> = Vec::new();
    backfill_stmts.push(emit_partitioned_backfill_only(group, part));
    let cascade_bf = emit_backfill_statements_cascade_only(group);
    backfill_stmts.extend(cascade_bf);

    // 3. Verification — runner short-circuits via PkFlipVerify
    //    labels.  Parent verification + cascade verification merged.
    let mut verify_stmts: Vec<OperationSql> = Vec::new();
    verify_stmts.push(emit_partitioned_verify(group));
    let cascade_verify = emit_verification_statements_cascade_only(group);
    verify_stmts.extend(cascade_verify);

    // 4. Concurrent indexes — parent UNIQUE placeholder (per-leaf
    //    expansion at apply time) + cascade member indexes. Self-FK
    //    columns on the partitioned parent take their own
    //    partitioned-aware path because `CREATE INDEX CONCURRENTLY`
    //    is rejected directly on partitioned parents.
    let mut index_stmts: Vec<OperationSql> = Vec::new();
    index_stmts.push(emit_partitioned_indexes(group, part));
    index_stmts.extend(emit_partitioned_self_fk_indexes(group));
    let cascade_idx = emit_concurrent_index_statements_cascade_only(group);
    index_stmts.extend(cascade_idx);

    // 5. Child FK creation (NOT VALID + VALIDATE) — same as the
    //    non-partitioned cascade flow.
    let fk_stmts = emit_child_fk_statements(group);

    // 6. NOT NULL proof — parent + non-nullable children.
    let nn_op = emit_not_null_proof(group);

    // 7. Cutover — partitioned parent + cascade finalisation in ONE
    //    transactional segment. The runner wraps the whole segment
    //    in a single Postgres tx; the body below is the statement
    //    list only.
    let cutover_op = emit_partitioned_cutover_with_cascade(group, part);

    let mut segments: Vec<Segment> = vec![
        Segment {
            kind: SegmentKind::Transactional,
            statements: vec![prep_op],
        },
        Segment {
            kind: SegmentKind::NonTransactional,
            statements: backfill_stmts,
        },
        Segment {
            kind: SegmentKind::Transactional,
            statements: verify_stmts,
        },
        Segment {
            kind: SegmentKind::NonTransactional,
            statements: index_stmts,
        },
    ];
    if !fk_stmts.is_empty() {
        segments.push(Segment {
            kind: SegmentKind::Transactional,
            statements: fk_stmts,
        });
    }
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![nn_op],
    });
    segments.push(Segment {
        kind: SegmentKind::Transactional,
        statements: vec![cutover_op],
    });
    segments
}

/// Cascade-only preparation: child and join-table shadow columns and
/// triggers, omitting parent-table work (which the partitioned emitter
/// owns). Returns an empty [`OperationSql`] when the group has no
/// cascade members.
fn emit_preparation_children_only(group: &PkTypeFlipGroup) -> OperationSql {
    emit_preparation_with_mode(group, EmitMode::CascadeOnly)
}

/// Cascade-only backfill: children, self-FK, join tables, cycle
/// peers. Omits the parent table (handled by the partitioned
/// per-leaf emitter).
fn emit_backfill_statements_cascade_only(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    emit_backfill_statements_with_mode(group, EmitMode::CascadeOnly)
}

/// Cascade-only verification: same shapes as
/// [`emit_verification_statements`] but excluding the parent PK
/// non-null check (the partitioned emitter owns that).
fn emit_verification_statements_cascade_only(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    emit_verification_statements_with_mode(group, EmitMode::CascadeOnly)
}

/// Cascade-only concurrent indexes: same shapes as
/// [`emit_concurrent_index_statements`] but excluding the parent's
/// own UNIQUE index (the partitioned emitter owns it via the
/// parent-level UNIQUE-on-ONLY placeholder + per-leaf
/// CONCURRENTLY + ATTACH PARTITION lines).
fn emit_concurrent_index_statements_cascade_only(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    emit_concurrent_index_statements_with_mode(group, EmitMode::CascadeOnly)
}

/// Partitioned cutover that ALSO finalises every cascade member.
/// Body: drop child FKs, partitioned parent promotion + DEFAULT
/// flip + drop of old PK column + RENAME, then per-child / join /
/// self-FK finalisation (DROP old col, DROP TRIGGER + FUNCTION,
/// RENAME shadow back, ADD CONSTRAINT pointing at the renamed
/// `parent.id`). All within ONE Postgres tx.
fn emit_partitioned_cutover_with_cascade(
    group: &PkTypeFlipGroup,
    _part: &super::diff::PkFlipPartitionedMeta,
) -> OperationSql {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let next_fn = next_fn_name(p_family, group.direction);
    let part_col = match &group.partitioned_parent {
        Some(meta) => match &meta.partition {
            PartitionSchema::Range { column } => column.clone(),
            PartitionSchema::Hash { column, .. } => column.clone(),
        },
        None => "partition_key".to_string(),
    };
    let mut up = String::new();

    // Cycle handling — defer all constraints if any cycles exist.
    if !group.cycles.is_empty() {
        up.push_str("SET CONSTRAINTS ALL DEFERRED;\n");
    }

    // 1. Drop every old FK.
    cutover_phase_drop_old_fks(group, &mut up);

    // 2. Promote the partitioned parent. ADD PRIMARY KEY (...) form
    //    because USING INDEX is illegal on a partitioned parent.
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} DROP CONSTRAINT {parent}_pkey;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ADD PRIMARY KEY ({pkey}, id{suffix});",
        parent = parent,
        pkey = part_col,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ALTER COLUMN id{suffix} SET DEFAULT {next}();",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        next = next_fn,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ALTER COLUMN id DROP DEFAULT;",
        parent = parent,
    );
    let _ = writeln!(up, "ALTER TABLE {parent} DROP COLUMN id;", parent = parent);
    let _ = writeln!(
        up,
        "DROP TRIGGER zzz_{parent}_autofill_desc ON {parent};",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "DROP FUNCTION zzz_{parent}_autofill_desc() CASCADE;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} RENAME COLUMN id{suffix} TO id;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );

    // 3. Self-FK column rename + constraint re-add. Mirrors the
    //    non-partitioned `cutover_phase_promote_parent` self-FK
    //    cleanup block — DROP segment-3b shadow constraint BEFORE
    //    rename (otherwise it survives under its `_desc_fkey` name
    //    on the renamed column, doubling the FK count) and preserve
    //    per-FK deferrability via `render_deferrable_clause`.
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "ALTER TABLE {parent} DROP COLUMN {col};",
                parent = parent,
                col = col,
            );
        }
        for col in &self_fk.fk_columns {
            let constraint = format!("{parent}_{col}{suffix}_fkey", suffix = SHADOW_SUFFIX);
            cutover_drop_constraint(&mut up, parent, &constraint);
        }
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "ALTER TABLE {parent} RENAME COLUMN {col}{suffix} TO {col};",
                parent = parent,
                col = col,
                suffix = SHADOW_SUFFIX,
            );
        }
        for (i, (col, cons)) in self_fk
            .fk_columns
            .iter()
            .zip(self_fk.fk_constraint_names.iter())
            .enumerate()
        {
            let deferrable_clause = render_deferrable_clause(
                self_fk.fk_deferrable.get(i).copied().unwrap_or(false),
                self_fk
                    .fk_initially_deferred
                    .get(i)
                    .copied()
                    .unwrap_or(false),
            );
            cutover_add_fk_constraint(&mut up, parent, cons, col, parent, "id", deferrable_clause);
        }
    }

    // 4. Finalise every child. Mirrors `cutover_phase_finalise_children`:
    //    DROP segment-3b shadow constraint BEFORE rename (else two FKs
    //    end up on the renamed column under different names) and
    //    preserve per-FK deferrability.
    cutover_phase_finalise_children(group, &mut up);

    // 5. Finalise every join table. Mirrors
    //    `cutover_phase_finalise_join_tables`: DROP segment-3b shadow
    //    constraint BEFORE rename and preserve per-pair deferrability
    //    (Option A cross-flipping uses partner-side flags).
    cutover_phase_finalise_join_tables(group, &mut up);

    OperationSql {
        label: format!("PkFlipPartitionedCutover {parent}"),
        up,
        down: format!(
            "-- POINT OF NO RETURN — partitioned cutover for {parent} cannot be\n\
             -- reversed by `down` SQL alone. Inverse migration required.",
        ),
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::PkTypeFlipPostCutover,
            detail: format!(
                "POINT OF NO RETURN: partitioned cutover for `{parent}` removes the prior \
                 PK column and trigger; rollback requires an inverse migration. \
                 Partitioned-table cutover is seconds-to-minutes class — benchmark first.",
            ),
        }),
    }
}

// ── Helpers shared across emitters ───────────────────────────────────────

/// Source-side column name on the parent — always `"id"` in current
/// Djogi (PK column is `id`).
const PARENT_PK_COLUMN: &str = "id";

/// Shadow column suffix added during the migration window. The
/// playbook calls this `_desc` in both forward (asc→desc) and
/// reverse (desc→asc) directions — the suffix names the migration
/// shape, not the final direction. We keep the suffix verbatim so
/// the playbook's named identifiers (`idx_tbl_id_desc`,
/// `nodes_parent_id_desc_fkey`, `zzz_<table>_autofill_desc`) survive
/// unchanged.
const SHADOW_SUFFIX: &str = "_desc";

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmitMode {
    Standard,
    CascadeOnly,
}

impl EmitMode {
    fn includes_parent(self) -> bool {
        matches!(self, EmitMode::Standard)
    }
}

/// SQL type for a HeerId / RanjId column at the wire level.
fn pg_id_type(family: PkFlipFamily) -> &'static str {
    match family {
        PkFlipFamily::Heer => "bigint",
        PkFlipFamily::Ranj => "uuid",
    }
}

/// Family for the parent — derived from the parent kind. Used as
/// the family for the parent's shadow column and trigger.
fn parent_family(group: &PkTypeFlipGroup) -> PkFlipFamily {
    match group.parent_from {
        super::schema::PkKindSchema::HeerId | super::schema::PkKindSchema::HeerIdRecencyBiased => {
            PkFlipFamily::Heer
        }
        super::schema::PkKindSchema::RanjId | super::schema::PkKindSchema::RanjIdRecencyBiased => {
            PkFlipFamily::Ranj
        }
        // Defensive — unreachable when the diff has already gated
        // the flip via `is_pk_kind_flip`. Default to Heer so the
        // emitter still produces SQL the operator can review (and
        // catch the mismatch from the type column rendered).
        _ => PkFlipFamily::Heer,
    }
}

/// Forward flip-fn name for the family + direction.
///
/// AscToDesc uses `heerid_to_desc` / `ranjid_to_desc`; DescToAsc
/// uses `heerid_to_asc` / `ranjid_to_asc`. The autofill trigger SQL
/// embeds this fn in its body.
fn flip_fn_name(family: PkFlipFamily, direction: PkFlipDirection) -> &'static str {
    match (family, direction) {
        (PkFlipFamily::Heer, PkFlipDirection::AscToDesc) => "heerid_to_desc",
        (PkFlipFamily::Heer, PkFlipDirection::DescToAsc) => "heerid_to_asc",
        (PkFlipFamily::Ranj, PkFlipDirection::AscToDesc) => "ranjid_to_desc",
        (PkFlipFamily::Ranj, PkFlipDirection::DescToAsc) => "ranjid_to_asc",
    }
}

/// Generator-default fn name for the new column DEFAULT after
/// cutover.
fn next_fn_name(family: PkFlipFamily, direction: PkFlipDirection) -> &'static str {
    match (family, direction) {
        (PkFlipFamily::Heer, PkFlipDirection::AscToDesc) => "heerid_next_desc",
        (PkFlipFamily::Heer, PkFlipDirection::DescToAsc) => "heerid_next",
        (PkFlipFamily::Ranj, PkFlipDirection::AscToDesc) => "ranjid_next_desc",
        (PkFlipFamily::Ranj, PkFlipDirection::DescToAsc) => "ranjid_next",
    }
}

/// `kind` argument passed to `heeranjid_bulk_backfill` — the
/// procedure dispatches on this string. The procedure exposes only
/// the desc direction; for reverse migrations the procedure calls
/// would use a parallel `_to_asc` procedure that the playbook
/// promises ships alongside. For Phase 7 T9 we always emit the
/// `'heer'` / `'ranj'` literal and rely on the procedure's flip-fn
/// dispatch — the desc-only procedure satisfies the asc→desc path
/// (the headline T9 case); the reverse path is unblocked by the
/// `migrate_asc_to_desc.rs` example wiring in HeeRanjID v0.3.x and
/// surfaces here as a runtime error if attempted before that
/// procedure pair is published. Operator-facing message is the SQL
/// emission itself — the procedure raises if `kind` is unknown.
fn backfill_kind_literal(family: PkFlipFamily) -> &'static str {
    match family {
        PkFlipFamily::Heer => "'heer'",
        PkFlipFamily::Ranj => "'ranj'",
    }
}

/// Children sharing the parent's family — the planner relies on
/// every child carrying the matching family. Defensively treats
/// mismatches as parent-family rather than panicking.
fn child_family(child: &PkFlipChild, group: &PkTypeFlipGroup) -> PkFlipFamily {
    if child.family as u8 == parent_family(group) as u8 {
        child.family
    } else {
        parent_family(group)
    }
}

/// `ON DELETE` rendering for an FK constraint clause.
fn render_on_delete(od: OnDeleteSchema) -> &'static str {
    match od {
        OnDeleteSchema::Restrict => "ON DELETE RESTRICT",
        OnDeleteSchema::Cascade => "ON DELETE CASCADE",
        OnDeleteSchema::SetNull => "ON DELETE SET NULL",
        OnDeleteSchema::SetDefault => "ON DELETE SET DEFAULT",
        OnDeleteSchema::NoAction => "ON DELETE NO ACTION",
    }
}

/// Render the autofill-trigger function body for a single table +
/// pair list. Mirrors the HeeRanjID `install_autofill_trigger_for_table`
/// helper exactly so `pg_dump` against a database where the runner
/// installed the trigger via the helper produces SQL byte-equal to
/// what this emitter produces — important for snapshot diffing.
///
/// Pairs is `&[(src_col, dst_col)]`. The function name follows
/// `zzz_<table>_autofill_desc` per the playbook's "load-bearing
/// `zzz_` prefix" convention.
fn render_autofill_trigger(
    table: &str,
    pairs: &[(&str, &str)],
    family: PkFlipFamily,
    direction: PkFlipDirection,
) -> String {
    let flip_fn = flip_fn_name(family, direction);
    let fn_name = format!("zzz_{}_autofill_desc", table);
    let mut insert_body = String::new();
    let mut update_body = String::new();
    for (src, dst) in pairs {
        let _ = writeln!(
            insert_body,
            "        IF NEW.{dst} IS NULL THEN NEW.{dst} := {flip}(NEW.{src}); END IF;",
            dst = dst,
            flip = flip_fn,
            src = src,
        );
        let _ = write!(
            update_body,
            "        IF NEW.{src} IS DISTINCT FROM OLD.{src} THEN\n            \
             NEW.{dst} := {flip}(NEW.{src});\n        ELSIF NEW.{dst} IS NULL THEN\n            \
             NEW.{dst} := {flip}(NEW.{src});\n        END IF;\n",
            src = src,
            dst = dst,
            flip = flip_fn,
        );
    }
    format!(
        "CREATE OR REPLACE FUNCTION {fn_name}() RETURNS trigger AS $body$\n\
         BEGIN\n    \
         IF TG_OP = 'INSERT' THEN\n\
         {insert_body}    \
         ELSIF TG_OP = 'UPDATE' THEN\n\
         {update_body}    \
         END IF;\n    \
         RETURN NEW;\n\
         END;\n\
         $body$ LANGUAGE plpgsql;\n\n\
         DROP TRIGGER IF EXISTS {fn_name} ON {table};\n\
         CREATE TRIGGER {fn_name}\n    \
         BEFORE INSERT OR UPDATE ON {table}\n    \
         FOR EACH ROW EXECUTE FUNCTION {fn_name}();\n",
        fn_name = fn_name,
        insert_body = insert_body,
        update_body = update_body,
        table = table,
    )
}

// ── Segment 1 — preparation ──────────────────────────────────────────────

fn emit_preparation(group: &PkTypeFlipGroup) -> OperationSql {
    emit_preparation_with_mode(group, EmitMode::Standard)
}

fn emit_preparation_with_mode(group: &PkTypeFlipGroup, mode: EmitMode) -> OperationSql {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let direction = group.direction;
    let id_type = pg_id_type(p_family);
    let mut up = String::new();
    let mut down = String::new();

    if mode.includes_parent() {
        // Parent shadow column.
        let _ = writeln!(
            up,
            "ALTER TABLE {parent} ADD COLUMN id{suffix} {ty};",
            parent = parent,
            suffix = SHADOW_SUFFIX,
            ty = id_type,
        );
        let _ = writeln!(
            down,
            "ALTER TABLE {parent} DROP COLUMN IF EXISTS id{suffix};",
            parent = parent,
            suffix = SHADOW_SUFFIX,
        );

        // Self-FK pairs add their own shadow columns. The NOT-VALID FK
        // pointing at the parent's `id_desc` lands in segment 3b after
        // the parent's CONCURRENT UNIQUE INDEX. Carrying both columns in
        // the same multi-pair trigger requires the columns themselves
        // here.
        let mut self_pairs: Vec<(String, String)> = Vec::new();
        self_pairs.push((PARENT_PK_COLUMN.to_string(), format!("id{}", SHADOW_SUFFIX)));
        if let Some(self_fk) = &group.self_fk {
            for (col, _cons) in self_fk
                .fk_columns
                .iter()
                .zip(self_fk.fk_constraint_names.iter())
            {
                let dst = format!("{col}{suffix}", col = col, suffix = SHADOW_SUFFIX);
                let _ = writeln!(
                    up,
                    "ALTER TABLE {parent} ADD COLUMN {dst} {ty};",
                    parent = parent,
                    dst = dst,
                    ty = id_type,
                );
                let _ = writeln!(
                    down,
                    "ALTER TABLE {parent} DROP COLUMN IF EXISTS {dst};",
                    parent = parent,
                    dst = dst,
                );
                self_pairs.push((col.clone(), dst));
            }
        }

        // Parent autofill trigger — multi-pair when self-FKs exist.
        let parent_pairs: Vec<(&str, &str)> = self_pairs
            .iter()
            .map(|(s, d)| (s.as_str(), d.as_str()))
            .collect();
        up.push_str(&render_autofill_trigger(
            parent,
            &parent_pairs,
            p_family,
            direction,
        ));
        let _ = writeln!(
            down,
            "DROP TRIGGER IF EXISTS zzz_{parent}_autofill_desc ON {parent};",
            parent = parent,
        );
        let _ = writeln!(
            down,
            "DROP FUNCTION IF EXISTS zzz_{parent}_autofill_desc() CASCADE;",
            parent = parent,
        );
    }

    // Children — shadow column + autofill trigger. The NOT-VALID FK
    // pointing at `parent(id_desc)` is NOT emitted here because
    // Postgres requires the target column to carry a unique
    // constraint at FK-creation time, and the parent's CONCURRENT
    // UNIQUE INDEX has not run yet. The FK lands in segment 3b
    // after the parent's index is built; see `emit_child_fks`.
    for child in &group.children {
        let cf = child_family(child, group);
        let cty = pg_id_type(cf);
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let _ = writeln!(
            up,
            "ALTER TABLE {child_t} ADD COLUMN {dst} {ty};",
            child_t = child.table,
            dst = dst,
            ty = cty,
        );
        up.push_str(&render_autofill_trigger(
            &child.table,
            &[(child.fk_column.as_str(), dst.as_str())],
            cf,
            direction,
        ));
        let _ = writeln!(
            down,
            "DROP TRIGGER IF EXISTS zzz_{child_t}_autofill_desc ON {child_t};",
            child_t = child.table,
        );
        let _ = writeln!(
            down,
            "DROP FUNCTION IF EXISTS zzz_{child_t}_autofill_desc() CASCADE;",
            child_t = child.table,
        );
        let _ = writeln!(
            down,
            "ALTER TABLE {child_t} DROP COLUMN IF EXISTS {dst};",
            child_t = child.table,
            dst = dst,
        );
    }

    // Join tables — shadow column + trigger. Same FK-deferral
    // reasoning as children above; the FK lands in segment 3b.
    // B-12: Option A + cross-flipping installs both pairs through
    // one multi-pair trigger; Option B installs only this group's
    // parent pair.
    for jt in &group.join_tables {
        let pairs = jt_shadow_pairs(jt, group);
        let id_type = pg_id_type(jt.family);
        for pair in &pairs {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let _ = writeln!(
                up,
                "ALTER TABLE {tbl} ADD COLUMN {dst} {ty};",
                tbl = jt.table,
                dst = dst,
                ty = id_type,
            );
        }
        let owned_pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|p| (p.col.to_string(), format!("{}{}", p.col, SHADOW_SUFFIX)))
            .collect();
        let pair_refs: Vec<(&str, &str)> = owned_pairs
            .iter()
            .map(|(c, d)| (c.as_str(), d.as_str()))
            .collect();
        up.push_str(&render_autofill_trigger(
            &jt.table, &pair_refs, jt.family, direction,
        ));
        let _ = writeln!(
            down,
            "DROP TRIGGER IF EXISTS zzz_{tbl}_autofill_desc ON {tbl};",
            tbl = jt.table,
        );
        let _ = writeln!(
            down,
            "DROP FUNCTION IF EXISTS zzz_{tbl}_autofill_desc() CASCADE;",
            tbl = jt.table,
        );
        for pair in &pairs {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let _ = writeln!(
                down,
                "ALTER TABLE {tbl} DROP COLUMN IF EXISTS {dst};",
                tbl = jt.table,
                dst = dst,
            );
        }
    }

    let label = match mode {
        EmitMode::Standard => format!("PkFlipPrep {parent}"),
        EmitMode::CascadeOnly => format!("PkFlipPrepCascade {parent}"),
    };
    OperationSql {
        label,
        up,
        down,
        lossy: None,
    }
}

/// One FK column the planner needs to orchestrate on a join table.
///
/// Drives B-12: under [`PkFlipJoinTableOption::OptionA`] a join
/// table whose `fk_to_partner_column` is `Some(_)` (cross-flipping
/// case, both parents migrating in the same delta) yields TWO
/// pairs from [`jt_shadow_pairs`] — the parent's FK column and the
/// partner's FK column — so every segment emitter (preparation,
/// backfill, concurrent index, NOT VALID FK at 3b, NOT NULL proof,
/// cutover) installs the full shadow-column orchestration for both
/// columns inside this group's transaction window. That is the
/// playbook §7 "single mega-transaction" shape.
///
/// Under [`PkFlipJoinTableOption::OptionB`] the function yields
/// ONLY the parent's pair — the partner column waits for the
/// partner parent's flip group to handle it sequentially. Smaller
/// transaction windows, easier to abort, intermediate state where
/// one shadow exists without the other is tolerated by the
/// trigger setup. That is the playbook §7 "sequential" shape.
///
/// When `fk_to_partner_column` is `None` (single-parent join, or
/// the cross-flipping case where the differ's
/// `apply_pk_flip_join_table_option` second pass already
/// transferred ownership to the winner under Option A), the
/// function yields the parent pair regardless of the option —
/// there is no partner to flip at all.
struct JoinTableShadowPair<'a> {
    col: &'a str,
    constraint: &'a str,
}

fn jt_shadow_pairs<'a>(
    jt: &'a super::diff::PkFlipJoinTable,
    group: &PkTypeFlipGroup,
) -> Vec<JoinTableShadowPair<'a>> {
    let mut out = vec![JoinTableShadowPair {
        col: jt.fk_to_parent_column.as_str(),
        constraint: jt.fk_to_parent_constraint.as_str(),
    }];
    let cross_flipping = jt.fk_to_partner_column.is_some();
    let option_a = matches!(
        group.join_table_option,
        super::diff::PkFlipJoinTableOption::OptionA
    );
    if cross_flipping
        && option_a
        && let (Some(pcol), Some(pcons)) = (
            jt.fk_to_partner_column.as_ref(),
            jt.fk_to_partner_constraint.as_ref(),
        )
    {
        out.push(JoinTableShadowPair {
            col: pcol.as_str(),
            constraint: pcons.as_str(),
        });
    }
    out
}

// `child_in_cycle` removed — segment 3b's FK-deferrability decision
// now reads from `PkFlipChild::fk_deferrable`
// / `PkFlipChild::fk_initially_deferred` directly. The differ's cycle
// path forces `(true, true)` upstream; non-cycle children pass their
// descriptor flags through unchanged. The previous heuristic looked up
// the cycle membership at SQL emission time and silently downgraded
// descriptor-deferrable plain children to non-deferrable.

// ── Segment 2 — backfill + verification ──────────────────────────────────

/// **Reference / test-fixture helper.** Builds the all-in-one
/// backfill SQL block for documentation and byte-equality regression
/// tests against the playbook. The production segment plan emits one
/// [`OperationSql`] per CALL via [`emit_backfill_statements`] — the
/// procedure's internal `COMMIT`s otherwise raise `2D000` when
/// wrapped in the implicit simple-query batch tx. This helper stays
/// in the module so reviewers can diff its output against playbook
/// §3.2 / §4.
#[allow(dead_code)]
fn emit_backfill_and_verification(group: &PkTypeFlipGroup) -> OperationSql {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let p_kind = backfill_kind_literal(p_family);
    let mut up = String::new();

    // Parent backfill.
    let _ = writeln!(
        up,
        "CALL heeranjid_bulk_backfill('{parent}', 'id', 'id{suffix}', {kind}, 10000);",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        kind = p_kind,
    );

    // Parent verification — non-nullable PK invariant from §3.3.
    let _ = writeln!(
        up,
        "SELECT count(*) FROM {parent} WHERE id{suffix} IS NULL;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "-- expect: 0 (verification halt point — runner aborts on count > 0)",
    );

    // Self-FK backfills (one CALL per self-FK pair, per §6).
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let dst = format!("{}{}", col, SHADOW_SUFFIX);
            let _ = writeln!(
                up,
                "CALL heeranjid_bulk_backfill('{parent}', '{col}', '{dst}', {kind}, 10000);",
                parent = parent,
                col = col,
                dst = dst,
                kind = p_kind,
            );
            // Self-FK shadow nullability matches the source — emit
            // the §3.3 NULL-tracking invariant query (catches both
            // missed rows and stale rows).
            let _ = writeln!(
                up,
                "SELECT count(*) FROM {parent}\n  \
                 WHERE ({col} IS NULL) IS DISTINCT FROM ({dst} IS NULL)\n     \
                 OR ({col} IS NOT NULL AND {dst} <> {flip}({col}));",
                parent = parent,
                col = col,
                dst = dst,
                flip = flip_fn_name(p_family, group.direction),
            );
            let _ = writeln!(up, "-- expect: 0");
        }
    }

    // Children backfills + invariant.
    for child in &group.children {
        let cf = child_family(child, group);
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let kind_lit = backfill_kind_literal(cf);
        let _ = writeln!(
            up,
            "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000);",
            tbl = child.table,
            src = child.fk_column,
            dst = dst,
            kind = kind_lit,
        );
        if child.fk_nullable {
            // Nullable FK — emit §3.3 NULL-tracking invariant.
            let _ = writeln!(
                up,
                "SELECT count(*) FROM {tbl}\n  \
                 WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL)\n     \
                 OR ({src} IS NOT NULL AND {dst} <> {flip}({src}));",
                tbl = child.table,
                src = child.fk_column,
                dst = dst,
                flip = flip_fn_name(cf, group.direction),
            );
        } else {
            // Non-nullable FK — same shape as the parent PK
            // verification.
            let _ = writeln!(
                up,
                "SELECT count(*) FROM {tbl} WHERE {dst} IS NULL;",
                tbl = child.table,
                dst = dst,
            );
        }
        let _ = writeln!(up, "-- expect: 0");
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} VALIDATE CONSTRAINT {tbl}_{dst}_fkey;",
            tbl = child.table,
            dst = dst,
        );
    }

    // Join tables — same backfill + invariant. B-12: walk every
    // shadow pair this group is responsible for under the active
    // option (Option A + cross-flipping → both pairs; Option B or
    // single-parent → parent pair only).
    for jt in &group.join_tables {
        let kind_lit = backfill_kind_literal(jt.family);
        for pair in jt_shadow_pairs(jt, group) {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let _ = writeln!(
                up,
                "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000);",
                tbl = jt.table,
                src = pair.col,
                dst = dst,
                kind = kind_lit,
            );
            let _ = writeln!(
                up,
                "SELECT count(*) FROM {tbl}\n  \
                 WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL)\n     \
                 OR ({src} IS NOT NULL AND {dst} <> {flip}({src}));",
                tbl = jt.table,
                src = pair.col,
                dst = dst,
                flip = flip_fn_name(jt.family, group.direction),
            );
            let _ = writeln!(up, "-- expect: 0");
            let _ = writeln!(
                up,
                "ALTER TABLE {tbl} VALIDATE CONSTRAINT {tbl}_{dst}_fkey;",
                tbl = jt.table,
                dst = dst,
            );
        }
    }

    // Cycle peers are now first-class children (`PkFlipChild::cycle_flag = true`);
    // their backfill emits via the children loop above. The standalone cycle
    // loop that lived here previously was redundant work and would have produced
    // duplicate CALL statements per peer.

    OperationSql {
        label: format!("PkFlipBackfill {parent}"),
        up,
        down: "-- Backfill is idempotent under `WHERE dst IS NULL`; the\n\
               -- down side has no inverse beyond dropping the shadow\n\
               -- column itself, which segment 1's down already covers."
            .to_string(),
        lossy: None,
    }
}

/// Emit one [`OperationSql`] per backfill statement (CALL or
/// hand-rolled DO block) so the runner can dispatch each through the
/// internal single-statement batch path. Without this split the simple-query
/// protocol wraps multiple statements in an implicit transaction;
/// the procedure's internal `COMMIT` then fires `2D000 invalid
/// transaction termination` per the playbook's "must not be wrapped
/// in pool.begin()" warning.
///
/// **Forward direction (Asc → Desc).** The HeeRanjID procedure
/// `heeranjid_bulk_backfill` ships with a `'heer'` / `'ranj'`
/// `kind` parameter that dispatches to `heerid_to_desc` /
/// `ranjid_to_desc` server-side. Forward backfills emit one CALL
/// per `(table, src_col, dst_col)` tuple.
///
/// **Reverse direction (Desc → Asc).** The shipped procedure does
/// NOT cover this — its `kind` switch only handles the desc flip
/// primitives. Rather than depend on an unreleased
/// `heeranjid_bulk_backfill_to_asc`, we emit a hand-rolled `DO $$
/// ... $$` block that mirrors the procedure's two-loop pattern:
/// fast-path with `SKIP LOCKED`, cleanup pass without. Both reissue
/// `SET LOCAL lock_timeout = '30s'` per batch (transaction-scoped).
/// This keeps the reverse path self-contained and reviewable.
fn emit_backfill_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    emit_backfill_statements_with_mode(group, EmitMode::Standard)
}

fn emit_backfill_statements_with_mode(
    group: &PkTypeFlipGroup,
    mode: EmitMode,
) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let direction = group.direction;
    let mut out: Vec<OperationSql> = Vec::new();

    let down_note = "-- Backfill is idempotent under `WHERE dst IS NULL`; the\n\
                     -- down side has no inverse beyond dropping the shadow\n\
                     -- column itself, which segment 1's down already covers."
        .to_string();

    if mode.includes_parent() {
        out.push(OperationSql {
            label: format!("PkFlipBackfill {parent}"),
            up: emit_backfill_body(
                parent,
                "id",
                &format!("id{}", SHADOW_SUFFIX),
                p_family,
                direction,
            ),
            down: down_note.clone(),
            lossy: None,
        });
    }

    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let dst = format!("{}{}", col, SHADOW_SUFFIX);
            out.push(OperationSql {
                label: format!("PkFlipBackfill {parent} {col}"),
                up: emit_backfill_body(parent, col, &dst, p_family, direction),
                down: down_note.clone(),
                lossy: None,
            });
        }
    }

    for child in &group.children {
        let cf = child_family(child, group);
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        out.push(OperationSql {
            label: format!("PkFlipBackfill {tbl}", tbl = child.table),
            up: emit_backfill_body(&child.table, &child.fk_column, &dst, cf, direction),
            down: down_note.clone(),
            lossy: None,
        });
        // VALIDATE CONSTRAINT lives in segment 3b alongside the
        // FK-creation it validates; the FK does not exist yet at
        // backfill time.
    }

    for jt in &group.join_tables {
        // B-12: per-pair backfill. Option A + cross-flipping issues
        // two CALLs per join table (one per FK column); Option B
        // issues one (this group's parent column).
        for pair in jt_shadow_pairs(jt, group) {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            out.push(OperationSql {
                label: format!("PkFlipBackfill {tbl} {col}", tbl = jt.table, col = pair.col,),
                up: emit_backfill_body(&jt.table, pair.col, &dst, jt.family, direction),
                down: down_note.clone(),
                lossy: None,
            });
            // VALIDATE CONSTRAINT lives in segment 3b alongside
            // the FK-creation it validates; the FK does not exist
            // yet at backfill time.
        }
    }

    // Cycle peers are first-class children with `cycle_flag = true`. The
    // children loop above already emits the backfill statement per peer;
    // the cycle-only loop that used to live here was redundant.

    out
}

/// Pick the right backfill body for `(family, direction)`.
///
/// AscToDesc → forward CALL into the shipped HeeRanjID procedure.
/// DescToAsc → hand-rolled DO block (the procedure ships only the
/// desc direction; see [`emit_reverse_backfill`] for the loop body).
fn emit_backfill_body(
    table: &str,
    src_col: &str,
    dst_col: &str,
    family: PkFlipFamily,
    direction: PkFlipDirection,
) -> String {
    match direction {
        PkFlipDirection::AscToDesc => {
            let kind = backfill_kind_literal(family);
            format!(
                "CALL heeranjid_bulk_backfill('{table}', '{src}', '{dst}', {kind}, 10000)",
                table = table,
                src = src_col,
                dst = dst_col,
                kind = kind,
            )
        }
        PkFlipDirection::DescToAsc => emit_reverse_backfill(table, src_col, dst_col, family),
    }
}

/// Hand-rolled DO block mirroring `heeranjid_bulk_backfill`'s
/// two-loop pattern but using the asc-direction flip primitives
/// (`heerid_to_asc` / `ranjid_to_asc`).
///
/// Loop 1 — fast path with `FOR UPDATE SKIP LOCKED`. Fires
/// `lock_timeout = '30s'` per batch (SET LOCAL is transaction-scoped
/// so it must be reissued after every COMMIT).
/// Loop 2 — cleanup pass without SKIP LOCKED to drain rows that were
/// always locked by a long-running concurrent transaction.
///
/// Both loops filter `WHERE <dst> IS NULL AND <src> IS NOT NULL` so
/// nullable FK shadows skip rows where the source is itself NULL.
///
/// The DO block carries its own COMMITs because LOOP-with-COMMIT in
/// PL/pgSQL is allowed only inside procedures or DO blocks at the
/// top level. The runner dispatches this block through the internal batch
/// path outside any explicit BEGIN, identical to the CALL path.
fn emit_reverse_backfill(
    table: &str,
    src_col: &str,
    dst_col: &str,
    family: PkFlipFamily,
) -> String {
    let flip_fn = match family {
        PkFlipFamily::Heer => "heerid_to_asc",
        PkFlipFamily::Ranj => "ranjid_to_asc",
    };
    format!(
        "DO $$\n\
         DECLARE\n    \
         rows_done int;\n\
         BEGIN\n    \
         LOOP\n        \
         SET LOCAL lock_timeout = '30s';\n        \
         WITH batch AS (\n            \
         SELECT ctid FROM {table}\n            \
         WHERE {dst} IS NULL AND {src} IS NOT NULL\n            \
         LIMIT 10000\n            \
         FOR UPDATE SKIP LOCKED\n        \
         )\n        \
         UPDATE {table} t SET {dst} = {flip}(t.{src})\n        \
         FROM batch WHERE t.ctid = batch.ctid;\n        \
         GET DIAGNOSTICS rows_done = ROW_COUNT;\n        \
         COMMIT;\n        \
         EXIT WHEN rows_done = 0;\n    \
         END LOOP;\n    \
         LOOP\n        \
         SET LOCAL lock_timeout = '30s';\n        \
         WITH batch AS (\n            \
         SELECT ctid FROM {table}\n            \
         WHERE {dst} IS NULL AND {src} IS NOT NULL\n            \
         LIMIT 10000\n            \
         FOR UPDATE\n        \
         )\n        \
         UPDATE {table} t SET {dst} = {flip}(t.{src})\n        \
         FROM batch WHERE t.ctid = batch.ctid;\n        \
         GET DIAGNOSTICS rows_done = ROW_COUNT;\n        \
         COMMIT;\n        \
         EXIT WHEN rows_done = 0;\n    \
         END LOOP;\n\
         END;\n\
         $$",
        table = table,
        dst = dst_col,
        src = src_col,
        flip = flip_fn,
    )
}

/// Emit one [`OperationSql`] per verification table the runner must
/// halt on. Labels are `PkFlipVerify <table> <hint>` so the runner's
/// transactional-segment dispatch recognises them, runs the SELECT
/// as a count-assert, and surfaces
/// [`super::runner::RunnerError::PkFlipVerificationFailed`] on any
/// non-zero count. The `up` body is the verification SQL verbatim.
/// The `down` body is empty — verification has no inverse.
fn emit_verification_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    emit_verification_statements_with_mode(group, EmitMode::Standard)
}

fn emit_verification_statements_with_mode(
    group: &PkTypeFlipGroup,
    mode: EmitMode,
) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let mut out: Vec<OperationSql> = Vec::new();

    if mode.includes_parent() {
        // Parent — non-nullable PK invariant from §3.3.
        out.push(OperationSql {
            label: format!("PkFlipVerify {parent} pk-non-null"),
            up: format!(
                "SELECT count(*) FROM {parent} WHERE id{suffix} IS NULL",
                parent = parent,
                suffix = SHADOW_SUFFIX,
            ),
            down: String::new(),
            lossy: None,
        });
    }

    // Self-FK pairs — §3.3 NULL-tracking invariant.
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let dst = format!("{}{}", col, SHADOW_SUFFIX);
            out.push(OperationSql {
                label: format!("PkFlipVerify {parent} {col}"),
                up: format!(
                    "SELECT count(*) FROM {parent} \
                     WHERE ({col} IS NULL) IS DISTINCT FROM ({dst} IS NULL) \
                        OR ({col} IS NOT NULL AND {dst} <> {flip}({col}))",
                    parent = parent,
                    col = col,
                    dst = dst,
                    flip = flip_fn_name(p_family, group.direction),
                ),
                down: String::new(),
                lossy: None,
            });
        }
    }

    // Children — choose nullable vs non-nullable shape.
    for child in &group.children {
        let cf = child_family(child, group);
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        if child.fk_nullable {
            out.push(OperationSql {
                label: format!(
                    "PkFlipVerify {tbl} {col}",
                    tbl = child.table,
                    col = child.fk_column
                ),
                up: format!(
                    "SELECT count(*) FROM {tbl} \
                     WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL) \
                        OR ({src} IS NOT NULL AND {dst} <> {flip}({src}))",
                    tbl = child.table,
                    src = child.fk_column,
                    dst = dst,
                    flip = flip_fn_name(cf, group.direction),
                ),
                down: String::new(),
                lossy: None,
            });
        } else {
            out.push(OperationSql {
                label: format!(
                    "PkFlipVerify {tbl} {col}",
                    tbl = child.table,
                    col = child.fk_column
                ),
                up: format!(
                    "SELECT count(*) FROM {tbl} WHERE {dst} IS NULL",
                    tbl = child.table,
                    dst = dst,
                ),
                down: String::new(),
                lossy: None,
            });
        }
    }

    // Join tables — same shape as nullable child. B-12: per-pair
    // verification. Option A + cross-flipping verifies both
    // shadow columns; Option B verifies only the parent's.
    for jt in &group.join_tables {
        for pair in jt_shadow_pairs(jt, group) {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            out.push(OperationSql {
                label: format!("PkFlipVerify {tbl} {col}", tbl = jt.table, col = pair.col),
                up: format!(
                    "SELECT count(*) FROM {tbl} \
                     WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL) \
                        OR ({src} IS NOT NULL AND {dst} <> {flip}({src}))",
                    tbl = jt.table,
                    src = pair.col,
                    dst = dst,
                    flip = flip_fn_name(jt.family, group.direction),
                ),
                down: String::new(),
                lossy: None,
            });
        }
    }

    out
}

/// Emit the segment 3b statements: child / self-FK / join-table
/// NOT VALID FK creation followed by VALIDATE CONSTRAINT. The FKs
/// reference `parent(id_desc)` which now carries a unique index
/// from segment 3, so Postgres accepts the FK creation. VALIDATE
/// runs immediately because backfill (segment 2) populated the
/// shadow columns.
fn emit_child_fk_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let mut out: Vec<OperationSql> = Vec::new();

    // Self-FK constraints — per-FK deferrability. The cycle path forces
    // deferrable + initially_deferred
    // upstream in the differ; descriptor-declared deferrable FKs
    // round-trip through the per-FK arrays. Pre-B-16 the gate was
    // `!group.cycles.is_empty()` and silently downgraded
    // descriptor-deferrable self-FKs to non-deferrable.
    if let Some(self_fk) = &group.self_fk {
        for (i, col) in self_fk.fk_columns.iter().enumerate() {
            // The self-FK shadow column was created in segment 1
            // alongside the parent's `id_desc`. Its name follows the
            // `<col>_desc` convention via the SHADOW_SUFFIX, which
            // the embedded format strings below interpolate directly
            // through `{col}{suffix}`. We do not need a separate
            // `dst` binding because the SQL builder reaches both
            // names via the format args.
            let deferrable_clause = render_deferrable_clause(
                self_fk.fk_deferrable.get(i).copied().unwrap_or(false),
                self_fk
                    .fk_initially_deferred
                    .get(i)
                    .copied()
                    .unwrap_or(false),
            );
            out.push(OperationSql {
                label: format!("PkFlipAddFk {parent} {col}"),
                up: format!(
                    "ALTER TABLE {parent} ADD CONSTRAINT {parent}_{col}{suffix}_fkey \
                     FOREIGN KEY ({col}{suffix}) REFERENCES {parent}(id{suffix}){cycle} NOT VALID",
                    parent = parent,
                    col = col,
                    suffix = SHADOW_SUFFIX,
                    cycle = deferrable_clause,
                ),
                down: format!(
                    "ALTER TABLE {parent} DROP CONSTRAINT IF EXISTS {parent}_{col}{suffix}_fkey",
                    parent = parent,
                    col = col,
                    suffix = SHADOW_SUFFIX,
                ),
                lossy: None,
            });
            out.push(OperationSql {
                label: format!("PkFlipValidateFk {parent} {col}"),
                up: format!(
                    "ALTER TABLE {parent} VALIDATE CONSTRAINT {parent}_{col}{suffix}_fkey",
                    parent = parent,
                    col = col,
                    suffix = SHADOW_SUFFIX,
                ),
                down: String::new(),
                lossy: None,
            });
        }
    }

    for child in &group.children {
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        // Per-child deferrability flags: the differ sets `(true, true)`
        // for cycle children and descriptor flags for plain children.
        // Previously this used the `child_in_cycle` heuristic and silently
        // downgraded descriptor-deferrable plain children.
        let cycle_clause =
            render_deferrable_clause(child.fk_deferrable, child.fk_initially_deferred);
        out.push(OperationSql {
            label: format!(
                "PkFlipAddFk {tbl} {col}",
                tbl = child.table,
                col = child.fk_column
            ),
            up: format!(
                "ALTER TABLE {tbl} ADD CONSTRAINT {tbl}_{dst}_fkey \
                 FOREIGN KEY ({dst}) REFERENCES {parent}(id{suffix}){cycle} NOT VALID",
                tbl = child.table,
                dst = dst,
                parent = parent,
                suffix = SHADOW_SUFFIX,
                cycle = cycle_clause,
            ),
            down: format!(
                "ALTER TABLE {tbl} DROP CONSTRAINT IF EXISTS {tbl}_{dst}_fkey",
                tbl = child.table,
                dst = dst,
            ),
            lossy: None,
        });
        out.push(OperationSql {
            label: format!(
                "PkFlipValidateFk {tbl} {col}",
                tbl = child.table,
                col = child.fk_column
            ),
            up: format!(
                "ALTER TABLE {tbl} VALIDATE CONSTRAINT {tbl}_{dst}_fkey",
                tbl = child.table,
                dst = dst,
            ),
            down: String::new(),
            lossy: None,
        });
    }

    for jt in &group.join_tables {
        // B-12: emit segment-3b NOT VALID FK + VALIDATE per pair.
        // The parent column references THIS group's parent's
        // `id_desc` shadow; the partner column (Option A cross-
        // flipping) references the PARTNER table's `id_desc` shadow
        // — that shadow exists because under Option A the partner
        // group also runs and `id_desc` lands in partner segment 1.
        for pair in jt_shadow_pairs(jt, group) {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let is_parent_side = pair.col == jt.fk_to_parent_column.as_str();
            let target = if is_parent_side {
                parent.to_string()
            } else {
                jt.fk_to_partner_table.clone().expect(
                    "PkFlipJoinTable invariant: fk_to_partner_table is Some \
                     whenever fk_to_partner_column is Some; jt_shadow_pairs \
                     only emits a partner-side pair when fk_to_partner_column \
                     is Some",
                )
            };
            // B-16: per-FK deferrability on segment-3b NOT VALID
            // FK creation. The parent-side and partner-side carry
            // their own flags (the differ populates them from the
            // `ForeignKeySchema.deferrable` /
            // `ForeignKeySchema.initially_deferred` fields).
            let (def, init_def) = if is_parent_side {
                (
                    jt.fk_to_parent_deferrable,
                    jt.fk_to_parent_initially_deferred,
                )
            } else {
                (
                    jt.fk_to_partner_deferrable,
                    jt.fk_to_partner_initially_deferred,
                )
            };
            let deferrable_clause = render_deferrable_clause(def, init_def);
            out.push(OperationSql {
                label: format!("PkFlipAddFk {tbl} {col}", tbl = jt.table, col = pair.col),
                up: format!(
                    "ALTER TABLE {tbl} ADD CONSTRAINT {tbl}_{dst}_fkey \
                     FOREIGN KEY ({dst}) REFERENCES {target}(id{suffix}){deferrable} NOT VALID",
                    tbl = jt.table,
                    dst = dst,
                    target = target,
                    suffix = SHADOW_SUFFIX,
                    deferrable = deferrable_clause,
                ),
                down: format!(
                    "ALTER TABLE {tbl} DROP CONSTRAINT IF EXISTS {tbl}_{dst}_fkey",
                    tbl = jt.table,
                    dst = dst,
                ),
                lossy: None,
            });
            out.push(OperationSql {
                label: format!(
                    "PkFlipValidateFk {tbl} {col}",
                    tbl = jt.table,
                    col = pair.col,
                ),
                up: format!(
                    "ALTER TABLE {tbl} VALIDATE CONSTRAINT {tbl}_{dst}_fkey",
                    tbl = jt.table,
                    dst = dst,
                ),
                down: String::new(),
                lossy: None,
            });
        }
    }

    out
}

/// Emit one [`OperationSql`] per CONCURRENTLY index. Each must run
/// in its own statement — concurrent index builds cannot run inside
/// any transaction, including the implicit simple-query batch tx
/// that fires when multiple statements share one `batch_execute`.
fn emit_concurrent_index_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    emit_concurrent_index_statements_with_mode(group, EmitMode::Standard)
}

fn emit_concurrent_index_statements_with_mode(
    group: &PkTypeFlipGroup,
    mode: EmitMode,
) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let mut out: Vec<OperationSql> = Vec::new();

    if mode.includes_parent() {
        out.push(OperationSql {
            label: format!("PkFlipConcurrentIndex {parent}"),
            up: format!(
                "CREATE UNIQUE INDEX CONCURRENTLY idx_{parent}_id{suffix} ON {parent} (id{suffix})",
                parent = parent,
                suffix = SHADOW_SUFFIX,
            ),
            down: format!(
                "DROP INDEX IF EXISTS idx_{parent}_id{suffix}",
                parent = parent,
                suffix = SHADOW_SUFFIX,
            ),
            lossy: None,
        });
    }

    // Self-FK indexes on the parent. When the parent is partitioned,
    // `CREATE INDEX CONCURRENTLY` directly on the partitioned parent
    // is rejected by Postgres — partitioned parents require the
    // `ON ONLY <parent>` + per-leaf `CONCURRENTLY` + `ATTACH PARTITION`
    // path. The partitioned flow handles those self-FK indexes via a
    // dedicated emitter (`emit_partitioned_self_fk_indexes`) routed
    // through the `PkFlipPartitionedSelfFkIndex` runner label, so we
    // skip them here when the parent is partitioned.
    if group.partitioned_parent.is_none()
        && let Some(self_fk) = &group.self_fk
    {
        for col in &self_fk.fk_columns {
            out.push(OperationSql {
                label: format!("PkFlipConcurrentIndex {parent} {col}"),
                up: format!(
                    "CREATE INDEX CONCURRENTLY idx_{parent}_{col}{suffix} ON {parent} ({col}{suffix})",
                    parent = parent,
                    col = col,
                    suffix = SHADOW_SUFFIX,
                ),
                down: format!(
                    "DROP INDEX IF EXISTS idx_{parent}_{col}{suffix}",
                    parent = parent,
                    col = col,
                    suffix = SHADOW_SUFFIX,
                ),
                lossy: None,
            });
        }
    }

    for child in &group.children {
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let unique_kw = if child.fk_unique { "UNIQUE " } else { "" };
        out.push(OperationSql {
            label: format!(
                "PkFlipConcurrentIndex {tbl} {col}",
                tbl = child.table,
                col = child.fk_column
            ),
            up: format!(
                "CREATE {uniq}INDEX CONCURRENTLY idx_{tbl}_{dst} ON {tbl} ({dst})",
                uniq = unique_kw,
                tbl = child.table,
                dst = dst,
            ),
            down: format!(
                "DROP INDEX IF EXISTS idx_{tbl}_{dst}",
                tbl = child.table,
                dst = dst,
            ),
            lossy: None,
        });
    }

    for jt in &group.join_tables {
        // B-12: per-pair concurrent index. Option A + cross-
        // flipping issues two CREATE INDEX CONCURRENTLY (one per
        // FK column); Option B issues one (the parent's column).
        for pair in jt_shadow_pairs(jt, group) {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            out.push(OperationSql {
                label: format!(
                    "PkFlipConcurrentIndex {tbl} {col}",
                    tbl = jt.table,
                    col = pair.col,
                ),
                up: format!(
                    "CREATE INDEX CONCURRENTLY idx_{tbl}_{dst} ON {tbl} ({dst})",
                    tbl = jt.table,
                    dst = dst,
                ),
                down: format!(
                    "DROP INDEX IF EXISTS idx_{tbl}_{dst}",
                    tbl = jt.table,
                    dst = dst,
                ),
                lossy: None,
            });
        }
    }

    out
}

// ── Segment 3 — concurrent unique indexes ────────────────────────────────

/// **Reference / test-fixture helper.** Builds the all-in-one
/// concurrent-index SQL block for documentation and byte-equality
/// regression tests against playbook §3.4 / §6 / §7. The production
/// segment plan emits one [`OperationSql`] per `CREATE INDEX
/// CONCURRENTLY` via [`emit_concurrent_index_statements`] — concurrent
/// builds cannot run inside any transaction, including the implicit
/// simple-query batch tx that fires when multiple statements share
/// one `batch_execute`.
#[allow(dead_code)]
fn emit_concurrent_indexes(group: &PkTypeFlipGroup) -> OperationSql {
    let parent = group.parent_table.as_str();
    let mut up = String::new();
    let mut down = String::new();

    // Parent — UNIQUE index (becomes the new PK in the cutover).
    let _ = writeln!(
        up,
        "CREATE UNIQUE INDEX CONCURRENTLY idx_{parent}_id{suffix} ON {parent} (id{suffix});",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        down,
        "DROP INDEX IF EXISTS idx_{parent}_id{suffix};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );

    // Self-FK columns — non-unique CONCURRENTLY index per §6
    // playbook example.
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "CREATE INDEX CONCURRENTLY idx_{parent}_{col}{suffix} ON {parent} ({col}{suffix});",
                parent = parent,
                col = col,
                suffix = SHADOW_SUFFIX,
            );
            let _ = writeln!(
                down,
                "DROP INDEX IF EXISTS idx_{parent}_{col}{suffix};",
                parent = parent,
                col = col,
                suffix = SHADOW_SUFFIX,
            );
        }
    }

    // Children — index on the FK shadow column. UNIQUE only when
    // the underlying FK column was UNIQUE (rare).
    for child in &group.children {
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let unique_kw = if child.fk_unique { "UNIQUE " } else { "" };
        let _ = writeln!(
            up,
            "CREATE {uniq}INDEX CONCURRENTLY idx_{tbl}_{dst} ON {tbl} ({dst});",
            uniq = unique_kw,
            tbl = child.table,
            dst = dst,
        );
        let _ = writeln!(
            down,
            "DROP INDEX IF EXISTS idx_{tbl}_{dst};",
            tbl = child.table,
            dst = dst,
        );
    }

    // Join tables — index on each FK shadow this group owns.
    // B-12: Option A + cross-flipping creates indexes on both
    // FK shadows; Option B / single-parent indexes only the
    // parent's shadow.
    for jt in &group.join_tables {
        for pair in jt_shadow_pairs(jt, group) {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let _ = writeln!(
                up,
                "CREATE INDEX CONCURRENTLY idx_{tbl}_{dst} ON {tbl} ({dst});",
                tbl = jt.table,
                dst = dst,
            );
            let _ = writeln!(
                down,
                "DROP INDEX IF EXISTS idx_{tbl}_{dst};",
                tbl = jt.table,
                dst = dst,
            );
        }
    }

    OperationSql {
        label: format!("PkFlipConcurrentIndex {parent}"),
        up,
        down,
        lossy: None,
    }
}

// ── Segment 4 — NOT NULL proof ────────────────────────────────────────────

fn emit_not_null_proof(group: &PkTypeFlipGroup) -> OperationSql {
    let parent = group.parent_table.as_str();
    let mut up = String::new();
    let mut down = String::new();

    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ADD CONSTRAINT {parent}_id{suffix}_nn \
         CHECK (id{suffix} IS NOT NULL) NOT VALID;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} VALIDATE CONSTRAINT {parent}_id{suffix}_nn;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ALTER COLUMN id{suffix} SET NOT NULL;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} DROP CONSTRAINT {parent}_id{suffix}_nn;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );

    // Children with non-nullable FK shadow get the same proof.
    for child in &group.children {
        if child.fk_nullable {
            continue;
        }
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} ADD CONSTRAINT {tbl}_{dst}_nn \
             CHECK ({dst} IS NOT NULL) NOT VALID;",
            tbl = child.table,
            dst = dst,
        );
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} VALIDATE CONSTRAINT {tbl}_{dst}_nn;",
            tbl = child.table,
            dst = dst,
        );
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} ALTER COLUMN {dst} SET NOT NULL;",
            tbl = child.table,
            dst = dst,
        );
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} DROP CONSTRAINT {tbl}_{dst}_nn;",
            tbl = child.table,
            dst = dst,
        );
    }

    // Down side: drop the NOT NULL on every column we tightened.
    let _ = writeln!(
        down,
        "ALTER TABLE {parent} ALTER COLUMN id{suffix} DROP NOT NULL;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    for child in &group.children {
        if child.fk_nullable {
            continue;
        }
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let _ = writeln!(
            down,
            "ALTER TABLE {tbl} ALTER COLUMN {dst} DROP NOT NULL;",
            tbl = child.table,
            dst = dst,
        );
    }

    OperationSql {
        label: format!("PkFlipNotNullProof {parent}"),
        up,
        down,
        lossy: None,
    }
}

// ── Segment 5 — cutover (POINT OF NO RETURN) ─────────────────────────────

fn emit_cutover(group: &PkTypeFlipGroup) -> OperationSql {
    let parent = group.parent_table.as_str();
    let mut up = String::new();

    // No `BEGIN;` here — the runner's
    // `run_transactional_segment` already wraps every statement in
    // a single Postgres transaction. Embedding `BEGIN;` here would
    // double-wrap and produce a `WARNING: there is already a
    // transaction in progress`. The segment is declared
    // `SegmentKind::Transactional` precisely so the runner owns the
    // tx boundary; the body below is the cutover statement list
    // only.

    // Cycle handling — defer all constraints if any cycles exist.
    // SET CONSTRAINTS ALL DEFERRED is the FIRST statement inside the
    // outer BEGIN the runner opens; deferred-FK peers on either side
    // of the cycle remain tolerant of intermediate FK states until
    // the runner-emitted `COMMIT` lands.
    if !group.cycles.is_empty() {
        up.push_str("SET CONSTRAINTS ALL DEFERRED;\n");
    }

    // The cutover body is split into composable phases so the
    // multi-parent emitter (`emit_cutover_multi`) can interleave
    // them across cluster members. The single-parent path
    // composes them in the same order [`build_segments`] expects;
    // the multi-parent path emits all members' phase-1 first, then
    // all members' phase-2, etc.
    cutover_phase_drop_old_fks(group, &mut up);
    cutover_phase_promote_parent(group, &mut up);
    cutover_phase_finalise_children(group, &mut up);
    cutover_phase_finalise_join_tables(group, &mut up);

    // No trailing `COMMIT;` — the runner emits the final COMMIT
    // for the transactional segment after every statement runs. See
    // the matching comment above the cycle-deferral block.

    let down = format!(
        "-- POINT OF NO RETURN — segment 5 (cutover) for {parent} cannot be\n\
         -- reversed by `down` SQL alone. Rollback requires an inverse\n\
         -- migration: add the previous-direction column back, install a\n\
         -- reverse autofill trigger, re-run heeranjid_bulk_backfill, and\n\
         -- run a second cutover. Plan that contingency BEFORE running\n\
         -- the forward cutover.",
        parent = parent,
    );

    OperationSql {
        label: format!("PkFlipCutover {parent}"),
        up,
        down,
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::PkTypeFlipPostCutover,
            detail: format!(
                "POINT OF NO RETURN: cutover for `{parent}` removes the prior PK column \
                 and trigger; rollback requires an inverse migration",
            ),
        }),
    }
}

// ── Cutover phases (composable for single + multi-parent paths) ──────────
//
// The cutover is a four-phase orchestration:
//
//   1. drop EVERY old FK pointing at the parent (children + join
//      tables + self-FK).
//   2. promote the parent — drop old PK, swap shadow → live, drop
//      old `id`, drop trigger / function, RENAME shadow → `id`.
//      Self-FK columns get the same drop / rename treatment.
//   3. finalise every child — drop old FK column, drop trigger /
//      function, RENAME shadow column → live name, ADD CONSTRAINT
//      pointing at the parent's new (post-rename) `id`.
//   4. finalise every join table this group owns — drop old FK
//      columns, drop trigger / function, RENAME shadow columns,
//      ADD CONSTRAINT for every pair pointing at the (post-rename)
//      parent / partner `id` columns.
//
// Single-parent path composes them in order. Multi-parent path
// emits phase 1 across all members, then phase 2 across all members,
// etc. — required because phase 4's
// `ADD CONSTRAINT (... REFERENCES partner(id))` assumes the
// partner's phase 2 has already run (phase 2 is what renames
// `id_desc` → `id`). Without interleaving, jt_books's phase 4
// would point at jt_tags's still-old `id` values; the FK check
// fires and Postgres rejects the cutover.

fn cutover_drop_constraint(up: &mut String, table: &str, constraint: &str) {
    let _ = writeln!(up, "ALTER TABLE {table} DROP CONSTRAINT {constraint};");
}

fn cutover_add_fk_constraint(
    up: &mut String,
    table: &str,
    constraint: &str,
    column: &str,
    target: &str,
    target_column: &str,
    trailing_clause: &str,
) {
    let _ = writeln!(
        up,
        "ALTER TABLE {table} ADD CONSTRAINT {constraint} \
         FOREIGN KEY ({column}) REFERENCES {target}({target_column}){trailing_clause};",
    );
}

/// Phase 1: drop every old FK pointing at the parent.
fn cutover_phase_drop_old_fks(group: &PkTypeFlipGroup, up: &mut String) {
    let parent = group.parent_table.as_str();
    for child in &group.children {
        cutover_drop_constraint(up, &child.table, &child.fk_constraint_name);
    }
    for jt in &group.join_tables {
        // B-12: drop EVERY FK on the join table this group owns.
        // Option A + cross-flipping drops both partner FKs (the
        // multi-group merger gives the winner ownership of the
        // join table); Option B / single-parent drops only the
        // parent's FK. The partner FK survives Option B's first
        // cutover and gets dropped in the partner's group cutover
        // later.
        for pair in jt_shadow_pairs(jt, group) {
            cutover_drop_constraint(up, &jt.table, pair.constraint);
        }
    }
    if let Some(self_fk) = &group.self_fk {
        for cons in &self_fk.fk_constraint_names {
            cutover_drop_constraint(up, parent, cons);
        }
    }
}

/// Phase 2: promote the parent — swap shadow PK to live PK.
fn cutover_phase_promote_parent(group: &PkTypeFlipGroup, up: &mut String) {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let direction = group.direction;
    let next_fn = next_fn_name(p_family, direction);

    let _ = writeln!(
        up,
        "ALTER TABLE {parent} DROP CONSTRAINT {parent}_pkey;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ADD CONSTRAINT {parent}_pkey \
         PRIMARY KEY USING INDEX idx_{parent}_id{suffix};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ALTER COLUMN id{suffix} SET DEFAULT {next}();",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        next = next_fn,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ALTER COLUMN id DROP DEFAULT;",
        parent = parent,
    );
    let _ = writeln!(up, "ALTER TABLE {parent} DROP COLUMN id;", parent = parent);
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "ALTER TABLE {parent} DROP COLUMN {col};",
                parent = parent,
                col = col,
            );
        }
    }
    let _ = writeln!(
        up,
        "DROP TRIGGER zzz_{parent}_autofill_desc ON {parent};",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "DROP FUNCTION zzz_{parent}_autofill_desc() CASCADE;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} RENAME COLUMN id{suffix} TO id;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    if let Some(self_fk) = &group.self_fk {
        // Drop segment-3b `_desc_fkey` constraints BEFORE the
        // rename, mirroring the children/join-table phase 3/4
        // logic — see the rationale on
        // `cutover_phase_finalise_children`.
        for col in &self_fk.fk_columns {
            let constraint = format!("{parent}_{col}{suffix}_fkey", suffix = SHADOW_SUFFIX);
            cutover_drop_constraint(up, parent, &constraint);
        }
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "ALTER TABLE {parent} RENAME COLUMN {col}{suffix} TO {col};",
                parent = parent,
                col = col,
                suffix = SHADOW_SUFFIX,
            );
        }
        // Re-add self-FK constraints with original names pointing at
        // the now-renamed shadow column (which is now the live `id`).
        // B-16: each self-FK preserves its declared deferrability —
        // cycle path forces `(true, true)`, plain self-FKs use
        // descriptor-side flags (default `(false, false)`).
        for (i, (col, cons)) in self_fk
            .fk_columns
            .iter()
            .zip(self_fk.fk_constraint_names.iter())
            .enumerate()
        {
            let deferrable_clause = render_deferrable_clause(
                self_fk.fk_deferrable.get(i).copied().unwrap_or(false),
                self_fk
                    .fk_initially_deferred
                    .get(i)
                    .copied()
                    .unwrap_or(false),
            );
            cutover_add_fk_constraint(up, parent, cons, col, parent, "id", deferrable_clause);
        }
    }
}

/// Phase 3: finalise every child — drop old FK column, drop the
/// `_desc_fkey` shadow constraint that segment 3b VALIDATEd, rename
/// shadow → live, ADD CONSTRAINT pointing at the parent's new `id`.
///
/// **Why drop the `_desc_fkey` constraint before re-adding the
/// canonical `_fkey`.** Segment 3b emitted the NOT VALID FK as
/// `<table>_<col>_desc_fkey` (named after the shadow column) and
/// VALIDATEd it. Postgres' `ALTER TABLE RENAME COLUMN` does NOT
/// rename constraints — after we RENAME `<col>_desc → <col>` the
/// `_desc_fkey` constraint is still attached to the column under
/// its original name. Without an explicit DROP we end up with
/// TWO FK constraints on the same column (one named `_desc_fkey`
/// from segment 3b, the other named `_fkey` from this phase).
/// They're functionally equivalent (both reference `parent(id)`,
/// both VALID) but the duplication pollutes `pg_constraint`,
/// confuses `\d` output, and breaks any test that counts FKs by
/// table. Dropping the `_desc_fkey` here keeps the post-cutover
/// schema clean.
///
/// **B-16 deferrability.** When the source FK was declared
/// `DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]`, the recreated FK
/// preserves the deferrable property by appending the matching
/// clause. Cycle peers force `deferrable = true, initially_deferred
/// = true` regardless of descriptor input — see `PkFlipChild`'s
/// type-level doc.
fn cutover_phase_finalise_children(group: &PkTypeFlipGroup, up: &mut String) {
    let parent = group.parent_table.as_str();
    for child in &group.children {
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} DROP COLUMN {col};",
            tbl = child.table,
            col = child.fk_column,
        );
        let _ = writeln!(
            up,
            "DROP TRIGGER zzz_{tbl}_autofill_desc ON {tbl};",
            tbl = child.table,
        );
        let _ = writeln!(
            up,
            "DROP FUNCTION zzz_{tbl}_autofill_desc() CASCADE;",
            tbl = child.table,
        );
        // Drop the segment-3b `_desc_fkey` constraint BEFORE the
        // rename. The constraint is on the shadow column (still
        // named `<col>_desc` at this point); the rename below
        // would otherwise leave it under its original name on the
        // newly-renamed column, doubling the FK count. The
        // matching segment 3b name is `<tbl>_<col>_desc_fkey`.
        let shadow_constraint = format!("{tbl}_{dst}_fkey", tbl = child.table);
        cutover_drop_constraint(up, &child.table, &shadow_constraint);
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} RENAME COLUMN {dst} TO {col};",
            tbl = child.table,
            dst = dst,
            col = child.fk_column,
        );
        let cascade = render_on_delete(child.on_delete);
        let deferrable_clause =
            render_deferrable_clause(child.fk_deferrable, child.fk_initially_deferred);
        let trailing_clause = format!(" {cascade}{deferrable_clause}");
        cutover_add_fk_constraint(
            up,
            &child.table,
            &child.fk_constraint_name,
            &child.fk_column,
            parent,
            "id",
            &trailing_clause,
        );
    }
}

/// Render the trailing `DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]`
/// clause for an FK ADD CONSTRAINT statement.
///
/// Returns the empty string when the FK is non-deferrable
/// (Postgres' default), `" DEFERRABLE INITIALLY IMMEDIATE"` for
/// `deferrable = true, initially_deferred = false`, or
/// `" DEFERRABLE INITIALLY DEFERRED"` for `deferrable = true,
/// initially_deferred = true`. The leading space lets callers
/// concatenate the result onto a `... <cascade>{clause}`
/// substring without a join helper.
///
/// Single source of truth for
/// the deferrability clause across cutover phase 3 (children),
/// phase 2 (self-FK re-add), and phase 4 (join-table re-add).
/// Centralising avoids drift across the three sites.
fn render_deferrable_clause(deferrable: bool, initially_deferred: bool) -> &'static str {
    match (deferrable, initially_deferred) {
        (false, _) => "",
        (true, true) => " DEFERRABLE INITIALLY DEFERRED",
        (true, false) => " DEFERRABLE INITIALLY IMMEDIATE",
    }
}

/// Phase 4: finalise every join table this group owns. Under
/// Option A + cross-flipping in a multi-parent cluster this fires
/// only on the winner; the loser's `join_tables` list is empty
/// after the merger transferred ownership, so this no-ops there.
fn cutover_phase_finalise_join_tables(group: &PkTypeFlipGroup, up: &mut String) {
    let parent = group.parent_table.as_str();
    // The trigger + function drop fires once per join table
    // because there's only one multi-pair trigger installed in
    // segment 1, regardless of pair count.
    for jt in &group.join_tables {
        let pairs = jt_shadow_pairs(jt, group);
        for pair in &pairs {
            let _ = writeln!(
                up,
                "ALTER TABLE {tbl} DROP COLUMN {col};",
                tbl = jt.table,
                col = pair.col,
            );
        }
        let _ = writeln!(
            up,
            "DROP TRIGGER zzz_{tbl}_autofill_desc ON {tbl};",
            tbl = jt.table,
        );
        let _ = writeln!(
            up,
            "DROP FUNCTION zzz_{tbl}_autofill_desc() CASCADE;",
            tbl = jt.table,
        );
        // Drop segment-3b `_desc_fkey` constraints before the
        // rename — same rationale as the children phase. The
        // segment-3b constraint name is
        // `<jt>_<pair_col>_desc_fkey` (the shadow column).
        for pair in &pairs {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let shadow_constraint = format!("{tbl}_{dst}_fkey", tbl = jt.table);
            cutover_drop_constraint(up, &jt.table, &shadow_constraint);
        }
        for pair in &pairs {
            let dst = format!("{}{}", pair.col, SHADOW_SUFFIX);
            let _ = writeln!(
                up,
                "ALTER TABLE {tbl} RENAME COLUMN {dst} TO {col};",
                tbl = jt.table,
                dst = dst,
                col = pair.col,
            );
        }
        for pair in &pairs {
            // First pair targets THIS group's parent; partner pair
            // (Option A + cross-flipping inside a multi-parent
            // cluster) targets the partner table recorded
            // alongside the partner column. The multi-parent
            // emitter ensures this runs AFTER every member's
            // phase 2, so the partner's `id` column is already
            // post-rename. B-16: per-FK deferrability — parent-
            // side and partner-side carry their own flags.
            let is_parent_side = pair.col == jt.fk_to_parent_column.as_str();
            let target_parent = if is_parent_side {
                parent.to_string()
            } else {
                jt.fk_to_partner_table.clone().expect(
                    "PkFlipJoinTable invariant: fk_to_partner_table is Some \
                     whenever fk_to_partner_column is Some; jt_shadow_pairs \
                     only emits a partner-side pair when fk_to_partner_column \
                     is Some",
                )
            };
            let (def, init_def) = if is_parent_side {
                (
                    jt.fk_to_parent_deferrable,
                    jt.fk_to_parent_initially_deferred,
                )
            } else {
                (
                    jt.fk_to_partner_deferrable,
                    jt.fk_to_partner_initially_deferred,
                )
            };
            let deferrable_clause = render_deferrable_clause(def, init_def);
            cutover_add_fk_constraint(
                up,
                &jt.table,
                pair.constraint,
                pair.col,
                &target_parent,
                "id",
                deferrable_clause,
            );
        }
        // Determinism marker for join-table layout. Each cutover
        // bears a comment line whose body identifies the operator
        // option in effect — Option A (single mega-tx covering
        // both parents' FK re-pointings on a cross-flipping join
        // table) or Option B (sequential per-parent flips with
        // the partner FK deferred to the partner parent's
        // cutover). The compose pipeline reads
        // `MigrateConfig::pk_flip_join_table_option` and flags
        // every emitted group via
        // [`apply_pk_flip_join_table_option`]; this comment makes
        // the operator-chosen layout visible in the rendered
        // cutover SQL so reviewers can confirm the generated
        // migration matches the intended layout without re-
        // reading TOML.
        let layout_label = match group.join_table_option {
            crate::migrate::diff::PkFlipJoinTableOption::OptionA => "OptionA",
            crate::migrate::diff::PkFlipJoinTableOption::OptionB => "OptionB",
        };
        let _ = writeln!(
            up,
            "-- Join-table layout: {layout_label} (parent={parent}, join_table={tbl})",
            tbl = jt.table,
        );
    }
}

// ── Partitioned-parent emitters (§9 of the playbook) ─────────────────────

fn emit_partitioned_preparation(
    group: &PkTypeFlipGroup,
    _part: &super::diff::PkFlipPartitionedMeta,
) -> OperationSql {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let direction = group.direction;
    let id_type = pg_id_type(p_family);
    let mut up = String::new();
    let mut down = String::new();

    // Parent shadow column. Postgres propagates the column to every
    // existing leaf via the shared parent storage layout; new
    // partitions created later inherit it as well.
    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ADD COLUMN id{suffix} {ty};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        ty = id_type,
    );
    let _ = writeln!(
        down,
        "ALTER TABLE {parent} DROP COLUMN IF EXISTS id{suffix};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );

    // Self-FK shadow columns + multi-pair trigger (mirrors the
    // non-partitioned `emit_preparation_with_mode` parent block).
    // When a partitioned parent has a self-FK, the autofill trigger
    // must populate BOTH the PK shadow column and every self-FK
    // shadow column from a single row insert; otherwise the cutover
    // phase tries to RENAME shadow columns that prep never created
    // and the segment fails to apply.
    let mut self_pairs: Vec<(String, String)> = Vec::new();
    self_pairs.push((PARENT_PK_COLUMN.to_string(), format!("id{}", SHADOW_SUFFIX)));
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let dst = format!("{col}{suffix}", col = col, suffix = SHADOW_SUFFIX);
            let _ = writeln!(
                up,
                "ALTER TABLE {parent} ADD COLUMN {dst} {ty};",
                parent = parent,
                dst = dst,
                ty = id_type,
            );
            let _ = writeln!(
                down,
                "ALTER TABLE {parent} DROP COLUMN IF EXISTS {dst};",
                parent = parent,
                dst = dst,
            );
            self_pairs.push((col.clone(), dst));
        }
    }

    let parent_pairs: Vec<(&str, &str)> = self_pairs
        .iter()
        .map(|(s, d)| (s.as_str(), d.as_str()))
        .collect();
    up.push_str(&render_autofill_trigger(
        parent,
        &parent_pairs,
        p_family,
        direction,
    ));
    let _ = writeln!(
        down,
        "DROP TRIGGER IF EXISTS zzz_{parent}_autofill_desc ON {parent};",
        parent = parent,
    );
    let _ = writeln!(
        down,
        "DROP FUNCTION IF EXISTS zzz_{parent}_autofill_desc() CASCADE;",
        parent = parent,
    );

    OperationSql {
        label: format!("PkFlipPartitionedPrep {parent}"),
        up,
        down,
        lossy: None,
    }
}

/// Emit the partitioned-parent backfill step ONLY (no verification).
///
/// The verification SELECT lives in a sibling [`emit_partitioned_verify`]
/// step so the runner's transactional-segment short-circuit picks it up
/// and halts on count > 0 via [`super::runner::RunnerError::PkFlipVerificationFailed`].
/// Bundling the SELECT into the backfill step would route it through
/// the non-returning internal batch path and discard the count silently (B-7).
fn emit_partitioned_backfill_only(
    group: &PkTypeFlipGroup,
    _part: &super::diff::PkFlipPartitionedMeta,
) -> OperationSql {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let direction = group.direction;
    let mut up = String::new();
    let _ = writeln!(
        up,
        "-- Partitioned parent: invoke the backfill primitive once per leaf\n\
         -- partition. The runner enumerates leaves from pg_inherits at apply\n\
         -- time and emits one statement per leaf in deterministic\n\
         -- regclass::text order, replacing the <EACH_LEAF_TABLE> placeholder\n\
         -- with the concrete partition name. Operators hand-running this\n\
         -- file MUST expand the placeholder themselves before executing.",
    );
    // Direction-aware body: forward dispatches to the shipped CALL,
    // reverse uses the hand-rolled DO block.
    let body = emit_backfill_body(
        "<EACH_LEAF_TABLE>",
        "id",
        &format!("id{}", SHADOW_SUFFIX),
        p_family,
        direction,
    );
    let _ = writeln!(up, "{body};", body = body);
    OperationSql {
        label: format!("PkFlipPartitionedBackfill {parent}"),
        up,
        down: "-- Partitioned backfill is idempotent under `WHERE dst IS NULL`;\n\
               -- the down side has no inverse beyond dropping the shadow column."
            .to_string(),
        lossy: None,
    }
}

/// Emit the partitioned-parent verification SELECT as a standalone
/// `PkFlipVerify` step. The runner's transactional-segment dispatcher
/// matches the `PkFlipVerify <table> ...` label prefix, runs the
/// statement as `query_one`, and surfaces
/// [`super::runner::RunnerError::PkFlipVerificationFailed`] on any
/// non-zero count. The SELECT runs against the partitioned parent and
/// aggregates across leaves automatically (Postgres routes through
/// pg_inherits transparently).
fn emit_partitioned_verify(group: &PkTypeFlipGroup) -> OperationSql {
    let parent = group.parent_table.as_str();
    OperationSql {
        label: format!("PkFlipVerify {parent} pk-non-null-aggregate"),
        up: format!(
            "SELECT count(*) FROM {parent} WHERE id{suffix} IS NULL",
            parent = parent,
            suffix = SHADOW_SUFFIX,
        ),
        down: String::new(),
        lossy: None,
    }
}

fn emit_partitioned_indexes(
    group: &PkTypeFlipGroup,
    _part: &super::diff::PkFlipPartitionedMeta,
) -> OperationSql {
    let parent = group.parent_table.as_str();
    let part_col = match &group.partitioned_parent {
        Some(meta) => match &meta.partition {
            PartitionSchema::Range { column } => column.clone(),
            PartitionSchema::Hash { column, .. } => column.clone(),
        },
        None => "partition_key".to_string(),
    };
    let mut up = String::new();
    let mut down = String::new();
    let _ = writeln!(
        up,
        "CREATE UNIQUE INDEX {parent}_{pkey}_id{suffix}_idx\n  \
         ON ONLY {parent} ({pkey}, id{suffix});",
        parent = parent,
        pkey = part_col,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "-- Per leaf: CREATE UNIQUE INDEX CONCURRENTLY <leaf>_{pkey}_id{suffix}_idx\n\
         --             ON <leaf> ({pkey}, id{suffix});\n\
         -- Then ALTER INDEX {parent}_{pkey}_id{suffix}_idx ATTACH PARTITION\n\
         --             <leaf>_{pkey}_id{suffix}_idx;\n\
         -- The runner enumerates leaves from pg_inherits and emits these\n\
         -- per-leaf statements at apply time.",
        pkey = part_col,
        suffix = SHADOW_SUFFIX,
        parent = parent,
    );
    let _ = writeln!(
        down,
        "DROP INDEX IF EXISTS {parent}_{pkey}_id{suffix}_idx;",
        parent = parent,
        pkey = part_col,
        suffix = SHADOW_SUFFIX,
    );
    OperationSql {
        label: format!("PkFlipPartitionedIndex {parent}"),
        up,
        down,
        lossy: None,
    }
}

/// Emit one [`OperationSql`] per self-FK shadow column on a
/// **partitioned** parent. Postgres rejects
/// `CREATE INDEX CONCURRENTLY` directly on a partitioned parent, so
/// the body carries the parent-level `CREATE INDEX <idx> ON ONLY
/// <parent> (<col>_desc)` plus a comment marker describing the
/// per-leaf expansion. The runner walks
/// `PkFlipPartitionedSelfFkIndex <parent>` labels at apply time
/// (one per self-FK column; the column itself is recovered from the
/// statement body via [`super::runner::recover_self_fk_column`]) and
/// emits one `CREATE INDEX CONCURRENTLY <leaf>_<col>_desc_idx
/// ON <leaf> (<col>_desc)` plus matching `ATTACH PARTITION` per
/// leaf.
///
/// Returns an empty vec when the group has no self-FK or when the
/// parent is not partitioned (caller should not call us in those
/// cases, but the guard is defensive).
fn emit_partitioned_self_fk_indexes(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    let mut out: Vec<OperationSql> = Vec::new();
    if group.partitioned_parent.is_none() {
        return out;
    }
    let Some(self_fk) = &group.self_fk else {
        return out;
    };
    let parent = group.parent_table.as_str();
    for col in &self_fk.fk_columns {
        let mut up = String::new();
        let _ = writeln!(
            up,
            "CREATE INDEX idx_{parent}_{col}{suffix}\n  \
             ON ONLY {parent} ({col}{suffix});",
            parent = parent,
            col = col,
            suffix = SHADOW_SUFFIX,
        );
        let _ = writeln!(
            up,
            "-- Per leaf: CREATE INDEX CONCURRENTLY <leaf>_{col}{suffix}_idx\n\
             --             ON <leaf> ({col}{suffix});\n\
             -- Then ALTER INDEX idx_{parent}_{col}{suffix} ATTACH PARTITION\n\
             --             <leaf>_{col}{suffix}_idx;\n\
             -- The runner enumerates leaves from pg_inherits and emits these\n\
             -- per-leaf statements at apply time.",
            parent = parent,
            col = col,
            suffix = SHADOW_SUFFIX,
        );
        let down = format!(
            "DROP INDEX IF EXISTS idx_{parent}_{col}{suffix};",
            parent = parent,
            col = col,
            suffix = SHADOW_SUFFIX,
        );
        out.push(OperationSql {
            label: format!("PkFlipPartitionedSelfFkIndex {parent}"),
            up,
            down,
            lossy: None,
        });
    }
    out
}

// `emit_partitioned_cutover` (the parent-only cutover for a
// partitioned flip without cascade members) was folded into
// [`emit_partitioned_cutover_with_cascade`] as part of B-4. The
// composed emitter handles both the cascade-empty and
// cascade-populated cases — when `group.children`, `self_fk`,
// `join_tables`, and `cycles` are all empty the body matches the
// original parent-only shape byte-for-byte.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::diff::{
        Classification, EnumVariantAnchor, PkFlipCycle, PkFlipJoinTable, PkFlipSelfFk,
        PkTypeFlipGroup, SchemaDelta, SchemaOperation, diff_bucket_maps,
    };
    use crate::migrate::projection::BucketKey;
    use crate::migrate::schema::{
        AppliedSchema, ColumnSchema, ForeignKeySchema, IndexSchema, OnDeleteSchema, PkKindSchema,
        PrimaryKeySchema, RelationKindSchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
    };
    use std::collections::BTreeMap;

    fn _silence_unused() {
        let _: Option<EnumVariantAnchor> = None;
        let _: Option<IndexSchema> = None;
    }

    fn empty_schema() -> AppliedSchema {
        AppliedSchema {
            djogi_version: String::new(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: String::new(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: Vec::new(),
        }
    }

    fn id_col() -> ColumnSchema {
        ColumnSchema {
            check: None,
            comment: None,
            default_sql: Some("heerid_next()".to_string()),
            foreign_key: None,
            generated: None,
            identity: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: "id".to_string(),
            nullable: false,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "BIGINT".to_string(),
            unique: false,
            type_change_using: None,
        }
    }

    fn id_col_desc() -> ColumnSchema {
        ColumnSchema {
            default_sql: Some("heerid_next_desc()".to_string()),
            ..id_col()
        }
    }

    fn fk_col(name: &str, target: &str, nullable: bool) -> ColumnSchema {
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
            indexed: true,
            max_length: None,
            name: name.to_string(),
            nullable,
            on_delete: Some(OnDeleteSchema::Restrict),
            outbox_exclude: false,
            rationale: None,
            relation_kind: Some(RelationKindSchema::ForeignKey),
            renamed_from: None,
            sequence_within: None,
            sql_type: "BIGINT".to_string(),
            unique: false,
            type_change_using: None,
        }
    }

    fn parent_table(name: &str, kind: PkKindSchema) -> TableSchema {
        let cols = vec![if matches!(kind, PkKindSchema::HeerIdRecencyBiased) {
            id_col_desc()
        } else {
            id_col()
        }];
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
                kind,
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

    fn child_table(
        name: &str,
        fk_target: &str,
        fk_col_name: &str,
        fk_nullable: bool,
    ) -> TableSchema {
        TableSchema {
            app: None,
            columns: vec![id_col(), fk_col(fk_col_name, fk_target, fk_nullable)],
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

    fn bucket() -> BucketKey {
        BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        }
    }

    fn bucket_map(s: AppliedSchema) -> BTreeMap<BucketKey, AppliedSchema> {
        let mut m = BTreeMap::new();
        m.insert(bucket(), s);
        m
    }

    // ── Differ pair detection ────────────────────────────────────────

    #[test]
    fn detects_all_four_pairs_via_diff_bucket_maps() {
        // HeerId → HeerIdRecencyBiased
        for (from, to) in [
            (PkKindSchema::HeerId, PkKindSchema::HeerIdRecencyBiased),
            (PkKindSchema::HeerIdRecencyBiased, PkKindSchema::HeerId),
            (PkKindSchema::RanjId, PkKindSchema::RanjIdRecencyBiased),
            (PkKindSchema::RanjIdRecencyBiased, PkKindSchema::RanjId),
        ] {
            let mut before = empty_schema();
            before
                .models
                .insert("authors".to_string(), parent_table("authors", from.clone()));
            let mut after = empty_schema();
            after
                .models
                .insert("authors".to_string(), parent_table("authors", to.clone()));
            let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
            let group_op = deltas
                .iter()
                .flat_map(|d| d.operations.iter())
                .find(|op| matches!(op, SchemaOperation::PkTypeFlipGroup(_)))
                .expect("group op present");
            if let SchemaOperation::PkTypeFlipGroup(g) = group_op {
                assert_eq!(g.parent_table, "authors");
                assert_eq!(g.parent_from, from);
                assert_eq!(g.parent_to, to);
            }
        }
    }

    #[test]
    fn non_flip_pk_change_not_misclassified() {
        // HeerId → Serial is not a supported flip; differ surfaces it
        // as Unsupported, never as a flip group.
        let mut before = empty_schema();
        before
            .models
            .insert("t".to_string(), parent_table("t", PkKindSchema::HeerId));
        let mut after = empty_schema();
        after
            .models
            .insert("t".to_string(), parent_table("t", PkKindSchema::Serial));
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
        let has_group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .any(|op| matches!(op, SchemaOperation::PkTypeFlipGroup(_)));
        assert!(
            !has_group,
            "Serial transition must not produce a flip group"
        );
        let has_unsupported = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .any(|op| matches!(op, SchemaOperation::Unsupported { .. }));
        assert!(has_unsupported);
    }

    #[test]
    fn fk_cascade_grouping_collects_children() {
        let mut before = empty_schema();
        before.models.insert(
            "authors".to_string(),
            parent_table("authors", PkKindSchema::HeerId),
        );
        before.models.insert(
            "books".to_string(),
            child_table("books", "authors", "author_id", false),
        );
        let mut after = empty_schema();
        after.models.insert(
            "authors".to_string(),
            parent_table("authors", PkKindSchema::HeerIdRecencyBiased),
        );
        after.models.insert(
            "books".to_string(),
            child_table("books", "authors", "author_id", false),
        );
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
        let group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group present");
        assert_eq!(group.children.len(), 1);
        assert_eq!(group.children[0].table, "books");
        assert_eq!(group.children[0].fk_column, "author_id");
        assert_eq!(group.children[0].family, PkFlipFamily::Heer);
    }

    #[test]
    fn self_fk_emits_multi_pair_trigger_metadata() {
        let mut nodes = parent_table("nodes", PkKindSchema::HeerId);
        nodes.columns.push(fk_col("parent_id", "nodes", true));
        let mut before = empty_schema();
        before.models.insert("nodes".to_string(), nodes.clone());
        let mut after_nodes = parent_table("nodes", PkKindSchema::HeerIdRecencyBiased);
        after_nodes.columns.push(fk_col("parent_id", "nodes", true));
        let mut after = empty_schema();
        after.models.insert("nodes".to_string(), after_nodes);
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
        let group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group present");
        let self_fk = group.self_fk.as_ref().expect("self_fk present");
        assert_eq!(self_fk.fk_columns, vec!["parent_id".to_string()]);
        assert!(group.children.is_empty());
    }

    #[test]
    fn join_table_grouping_detects_through_table() {
        let book_tags = TableSchema {
            app: None,
            columns: vec![
                fk_col("book_id", "books", false),
                fk_col("tag_id", "tags", false),
            ],
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: true,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["book_id".to_string(), "tag_id".to_string()],
                kind: PkKindSchema::Composite,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "book_tags".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        };
        let mut before = empty_schema();
        before.models.insert(
            "tags".to_string(),
            parent_table("tags", PkKindSchema::HeerId),
        );
        before
            .models
            .insert("book_tags".to_string(), book_tags.clone());
        let mut after = empty_schema();
        after.models.insert(
            "tags".to_string(),
            parent_table("tags", PkKindSchema::HeerIdRecencyBiased),
        );
        after.models.insert("book_tags".to_string(), book_tags);
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
        let group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group present");
        assert_eq!(group.join_tables.len(), 1);
        assert_eq!(group.join_tables[0].table, "book_tags");
        assert_eq!(group.join_tables[0].fk_to_parent_column, "tag_id");
    }

    #[test]
    fn cycle_detection_via_mutual_fks() {
        let mut a = parent_table("a", PkKindSchema::HeerId);
        a.columns.push(fk_col("b_id", "b", true));
        let mut b = parent_table("b", PkKindSchema::HeerId);
        b.columns.push(fk_col("a_id", "a", true));
        let mut before = empty_schema();
        before.models.insert("a".to_string(), a.clone());
        before.models.insert("b".to_string(), b.clone());
        let mut after_a = parent_table("a", PkKindSchema::HeerIdRecencyBiased);
        after_a.columns.push(fk_col("b_id", "b", true));
        let mut after = empty_schema();
        after.models.insert("a".to_string(), after_a);
        after.models.insert("b".to_string(), b);
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
        let group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group present");
        assert_eq!(group.cycles.len(), 1);
        assert_eq!(group.cycles[0].peer_table, "b");
        assert_eq!(group.cycles[0].peer_fk_column, "a_id");
        assert_eq!(group.cycles[0].self_fk_column, "b_id");
    }

    // ── SQL byte-equality regressions vs playbook §3 ─────────────────

    fn synth_group_single_table() -> PkTypeFlipGroup {
        PkTypeFlipGroup {
            parent_table: "tbl".to_string(),
            parent_from: PkKindSchema::HeerId,
            parent_to: PkKindSchema::HeerIdRecencyBiased,
            direction: PkFlipDirection::AscToDesc,
            children: Vec::new(),
            self_fk: None,
            join_tables: Vec::new(),
            cycles: Vec::new(),
            partitioned_parent: None,
            co_destructive: false,
            co_lossy: false,
            join_table_option: crate::migrate::diff::PkFlipJoinTableOption::OptionA,
        }
    }

    fn whitespace_normalize(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_was_ws = true;
        for &b in s.as_bytes() {
            let is_ws = matches!(b, b' ' | b'\t' | b'\n' | b'\r');
            if is_ws {
                if !prev_was_ws {
                    out.push(' ');
                    prev_was_ws = true;
                }
            } else {
                out.push(b as char);
                prev_was_ws = false;
            }
        }
        out.trim().to_string()
    }

    #[test]
    fn emitter_output_drift_check_section_3_preparation() {
        let group = synth_group_single_table();
        let prep = emit_preparation(&group);
        let normalised = whitespace_normalize(&prep.up);
        // Playbook §3.1: "ALTER TABLE tbl ADD COLUMN id_desc bigint;"
        assert!(
            normalised.contains("ALTER TABLE tbl ADD COLUMN id_desc bigint;"),
            "missing §3.1 ADD COLUMN; got: {normalised}",
        );
        // Trigger function name + heerid_to_desc must appear.
        assert!(normalised.contains("zzz_tbl_autofill_desc"));
        assert!(normalised.contains("heerid_to_desc(NEW.id)"));
    }

    #[test]
    fn emitter_output_drift_check_section_3_backfill() {
        let group = synth_group_single_table();
        let bf = emit_backfill_and_verification(&group);
        let n = whitespace_normalize(&bf.up);
        // §3.2 verbatim CALL form.
        assert!(
            n.contains("CALL heeranjid_bulk_backfill('tbl', 'id', 'id_desc', 'heer', 10000);"),
            "missing §3.2 CALL; got: {n}",
        );
        // §3.3 verification SELECT for the non-nullable PK case.
        assert!(
            n.contains("SELECT count(*) FROM tbl WHERE id_desc IS NULL;"),
            "missing §3.3 verification; got: {n}",
        );
    }

    #[test]
    fn emitter_output_drift_check_section_3_concurrent_index() {
        let group = synth_group_single_table();
        let idx = emit_concurrent_indexes(&group);
        let n = whitespace_normalize(&idx.up);
        assert!(
            n.contains("CREATE UNIQUE INDEX CONCURRENTLY idx_tbl_id_desc ON tbl (id_desc);"),
            "missing §3.4 concurrent index; got: {n}",
        );
    }

    #[test]
    fn emitter_output_drift_check_section_3_not_null_proof() {
        let group = synth_group_single_table();
        let proof = emit_not_null_proof(&group);
        let n = whitespace_normalize(&proof.up);
        assert!(n.contains(
            "ALTER TABLE tbl ADD CONSTRAINT tbl_id_desc_nn CHECK (id_desc IS NOT NULL) NOT VALID;"
        ));
        assert!(n.contains("ALTER TABLE tbl VALIDATE CONSTRAINT tbl_id_desc_nn;"));
        assert!(n.contains("ALTER TABLE tbl ALTER COLUMN id_desc SET NOT NULL;"));
        assert!(n.contains("ALTER TABLE tbl DROP CONSTRAINT tbl_id_desc_nn;"));
    }

    #[test]
    fn emitter_output_drift_check_section_3_cutover() {
        let group = synth_group_single_table();
        let cut = emit_cutover(&group);
        let n = whitespace_normalize(&cut.up);
        // Cutover sequence statements per §3.6.
        assert!(n.contains("ALTER TABLE tbl DROP CONSTRAINT tbl_pkey;"));
        assert!(n.contains(
            "ALTER TABLE tbl ADD CONSTRAINT tbl_pkey PRIMARY KEY USING INDEX idx_tbl_id_desc;"
        ));
        assert!(n.contains("ALTER TABLE tbl ALTER COLUMN id_desc SET DEFAULT heerid_next_desc();"));
        assert!(n.contains("ALTER TABLE tbl ALTER COLUMN id DROP DEFAULT;"));
        assert!(n.contains("ALTER TABLE tbl DROP COLUMN id;"));
        assert!(n.contains("DROP TRIGGER zzz_tbl_autofill_desc ON tbl;"));
        assert!(n.contains("DROP FUNCTION zzz_tbl_autofill_desc() CASCADE;"));
        assert!(n.contains("ALTER TABLE tbl RENAME COLUMN id_desc TO id;"));
        // Cutover body MUST NOT carry its own BEGIN/COMMIT — the
        // runner's `run_transactional_segment` wraps every statement
        // in a single Postgres tx already. Embedding our own pair
        // here would double-wrap and produce `WARNING: there is
        // already a transaction in progress` (B-9).
        assert!(
            !n.contains("BEGIN;"),
            "cutover body must not carry BEGIN; got: {n}"
        );
        assert!(
            !n.contains("COMMIT;"),
            "cutover body must not carry COMMIT; got: {n}"
        );
        // Lossy marker for the point-of-no-return.
        let warn = cut.lossy.expect("cutover lossy warning");
        assert_eq!(warn.kind, LossyRollbackKind::PkTypeFlipPostCutover);
    }

    // ── §4 parent + child ────────────────────────────────────────────

    #[test]
    fn emitter_output_drift_check_section_4_parent_child() {
        let mut group = synth_group_single_table();
        group.children.push(PkFlipChild {
            table: "c".to_string(),
            fk_column: "p_id".to_string(),
            fk_constraint_name: "c_p_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            fk_deferrable: false,
            fk_initially_deferred: false,
            fk_nullable: false,
            fk_unique: false,
            family: PkFlipFamily::Heer,
            cycle_flag: false,
        });
        // The parent in §4 is named "parent"; we use "tbl" for the
        // single-table fixture. Per the v3 segment plan the child FK
        // NOT VALID is emitted in segment 3b (after parent's
        // CONCURRENT UNIQUE INDEX commits) — Postgres requires the
        // FK target to be uniquely keyed at FK-creation time, even
        // with NOT VALID.
        let prep = emit_preparation(&group);
        let nprep = whitespace_normalize(&prep.up);
        assert!(nprep.contains("ALTER TABLE c ADD COLUMN p_id_desc bigint;"));
        assert!(
            !nprep.contains("ADD CONSTRAINT c_p_id_desc_fkey"),
            "child FK creation belongs in segment 3b, not segment 1"
        );

        let fk_stmts = emit_child_fk_statements(&group);
        let fk_text: String = fk_stmts
            .iter()
            .map(|s| s.up.as_str())
            .collect::<Vec<_>>()
            .join(";\n");
        let nfk = whitespace_normalize(&fk_text);
        assert!(nfk.contains(
            "ALTER TABLE c ADD CONSTRAINT c_p_id_desc_fkey \
             FOREIGN KEY (p_id_desc) REFERENCES tbl(id_desc) NOT VALID"
        ));
        assert!(nfk.contains("ALTER TABLE c VALIDATE CONSTRAINT c_p_id_desc_fkey"));

        let bf_stmts = emit_backfill_statements(&group);
        let bf_text: String = bf_stmts
            .iter()
            .map(|s| s.up.as_str())
            .collect::<Vec<_>>()
            .join(";\n");
        let nbf = whitespace_normalize(&bf_text);
        assert!(
            nbf.contains("CALL heeranjid_bulk_backfill('c', 'p_id', 'p_id_desc', 'heer', 10000)")
        );

        let cut = emit_cutover(&group);
        let ncut = whitespace_normalize(&cut.up);
        assert!(ncut.contains("ALTER TABLE c DROP CONSTRAINT c_p_id_fkey;"));
        // Re-add of the FK with original cascade discipline.
        assert!(ncut.contains(
            "ALTER TABLE c ADD CONSTRAINT c_p_id_fkey FOREIGN KEY (p_id) REFERENCES tbl(id) ON DELETE RESTRICT;"
        ));
    }

    // ── §6 self-FK ───────────────────────────────────────────────────

    #[test]
    fn emitter_output_drift_check_section_6_self_fk() {
        let mut group = synth_group_single_table();
        group.parent_table = "nodes".to_string();
        group.self_fk = Some(PkFlipSelfFk {
            fk_columns: vec!["parent_id".to_string()],
            fk_constraint_names: vec!["nodes_parent_id_fkey".to_string()],
            fk_deferrable: vec![false],
            fk_initially_deferred: vec![false],
        });
        let prep = emit_preparation(&group);
        let n = whitespace_normalize(&prep.up);
        // Multi-pair shadow columns + multi-pair trigger body in
        // segment 1; the self-FK constraint lands in segment 3b
        // (Postgres requires the target column to be uniquely keyed
        // at FK-creation time).
        assert!(n.contains("ALTER TABLE nodes ADD COLUMN id_desc bigint;"));
        assert!(n.contains("ALTER TABLE nodes ADD COLUMN parent_id_desc bigint;"));
        assert!(
            !n.contains("ADD CONSTRAINT nodes_parent_id_desc_fkey"),
            "self-FK constraint belongs in segment 3b"
        );
        // Multi-pair trigger body has both heerid_to_desc(NEW.id) and
        // heerid_to_desc(NEW.parent_id) lines.
        assert!(n.contains("heerid_to_desc(NEW.id)"));
        assert!(n.contains("heerid_to_desc(NEW.parent_id)"));
        // Self-FK constraint lands in segment 3b.
        let fk_stmts = emit_child_fk_statements(&group);
        let nfk = whitespace_normalize(
            &fk_stmts
                .iter()
                .map(|s| s.up.as_str())
                .collect::<Vec<_>>()
                .join(";\n"),
        );
        assert!(nfk.contains(
            "ALTER TABLE nodes ADD CONSTRAINT nodes_parent_id_desc_fkey \
             FOREIGN KEY (parent_id_desc) REFERENCES nodes(id_desc) NOT VALID"
        ));
        let cut = emit_cutover(&group);
        let ncut = whitespace_normalize(&cut.up);
        assert!(ncut.contains("ALTER TABLE nodes DROP CONSTRAINT nodes_parent_id_fkey;"));
        assert!(ncut.contains("ALTER TABLE nodes DROP CONSTRAINT nodes_pkey;"));
        assert!(ncut.contains(
            "ALTER TABLE nodes ADD CONSTRAINT nodes_pkey PRIMARY KEY USING INDEX idx_nodes_id_desc;"
        ));
        assert!(ncut.contains("ALTER TABLE nodes RENAME COLUMN parent_id_desc TO parent_id;"));
        assert!(ncut.contains(
            "ALTER TABLE nodes ADD CONSTRAINT nodes_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES nodes(id);"
        ));
    }

    // ── §7 join tables ───────────────────────────────────────────────

    #[test]
    fn emitter_output_drift_check_section_7_join_table() {
        let mut group = synth_group_single_table();
        group.parent_table = "tags".to_string();
        group.join_tables.push(PkFlipJoinTable {
            table: "book_tags".to_string(),
            fk_to_parent_column: "tag_id".to_string(),
            fk_to_parent_constraint: "book_tags_tag_id_fkey".to_string(),
            fk_to_parent_deferrable: false,
            fk_to_parent_initially_deferred: false,
            fk_to_partner_column: None,
            fk_to_partner_constraint: None,
            fk_to_partner_table: None,
            fk_to_partner_deferrable: false,
            fk_to_partner_initially_deferred: false,
            family: PkFlipFamily::Heer,
        });
        let prep = emit_preparation(&group);
        let n = whitespace_normalize(&prep.up);
        assert!(n.contains("ALTER TABLE book_tags ADD COLUMN tag_id_desc bigint;"));
        assert!(n.contains("zzz_book_tags_autofill_desc"));
        let bf = emit_backfill_and_verification(&group);
        let nbf = whitespace_normalize(&bf.up);
        assert!(nbf.contains(
            "CALL heeranjid_bulk_backfill('book_tags', 'tag_id', 'tag_id_desc', 'heer', 10000);"
        ));
        let cut = emit_cutover(&group);
        let ncut = whitespace_normalize(&cut.up);
        assert!(ncut.contains("ALTER TABLE book_tags DROP CONSTRAINT book_tags_tag_id_fkey;"));
        assert!(ncut.contains("ALTER TABLE book_tags RENAME COLUMN tag_id_desc TO tag_id;"));
    }

    // ── §8 cycles ─────────────────────────────────────────────────────

    #[test]
    fn emitter_output_drift_check_section_8_cycles_uses_deferrable() {
        let mut group = synth_group_single_table();
        group.parent_table = "a".to_string();
        group.children.push(PkFlipChild {
            table: "b".to_string(),
            fk_column: "a_id".to_string(),
            fk_constraint_name: "b_a_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            // Cycle peers force deferrable + initially_deferred; the
            // differ does this in `promote_pk_flips_to_groups` and
            // synth fixtures must match the production shape.
            fk_deferrable: true,
            fk_initially_deferred: true,
            fk_nullable: true,
            fk_unique: false,
            family: PkFlipFamily::Heer,
            // B-13: real differ output marks cycle peers as
            // first-class children with cycle_flag = true.
            cycle_flag: true,
        });
        group.cycles.push(PkFlipCycle {
            peer_table: "b".to_string(),
            peer_fk_column: "a_id".to_string(),
            self_fk_column: "b_id".to_string(),
        });
        let cut = emit_cutover(&group);
        let n = whitespace_normalize(&cut.up);
        assert!(
            n.contains("SET CONSTRAINTS ALL DEFERRED;"),
            "cycle cutover must defer constraints; got: {n}"
        );
    }

    // ── §9 partitioned ────────────────────────────────────────────────

    #[test]
    fn emitter_output_drift_check_section_9_partitioned_uses_add_primary_key() {
        let mut group = synth_group_single_table();
        group.parent_table = "events".to_string();
        group.partitioned_parent = Some(super::super::diff::PkFlipPartitionedMeta {
            partition: PartitionSchema::Range {
                column: "ts".to_string(),
            },
        });
        let segments = build_segments(&group).expect("build_segments");
        // B-7 split: segments are [prep, backfill, verify, index,
        // not_null_proof, cutover]. The verification SELECT is its
        // own `PkFlipVerify` segment so the runner's transactional
        // dispatcher halts on count > 0.
        assert_eq!(segments.len(), 6, "expected 6 segments post-B-7 split");
        let cut_stmt = &segments.last().expect("cutover segment").statements[0];
        let n = whitespace_normalize(&cut_stmt.up);
        assert!(
            n.contains("ALTER TABLE events ADD PRIMARY KEY (ts, id_desc);"),
            "partitioned cutover must use ADD PRIMARY KEY (...) form (not USING INDEX); got: {n}"
        );
        assert!(
            !n.contains("BEGIN;"),
            "partitioned cutover body must not carry BEGIN (B-9); got: {n}"
        );
        // Verify segment carries the PkFlipVerify label so the runner
        // intercepts via the count-assert short-circuit (B-7).
        let verify_stmt = &segments[2].statements[0];
        assert!(
            verify_stmt.label.starts_with("PkFlipVerify "),
            "expected verify segment with PkFlipVerify label; got: {}",
            verify_stmt.label
        );
        // Index segment (position 3 after the verify split) must
        // reference parent-level UNIQUE placeholder.
        let idx_stmt = &segments[3].statements[0];
        let nidx = whitespace_normalize(&idx_stmt.up);
        assert!(
            nidx.contains(
                "CREATE UNIQUE INDEX events_ts_id_desc_idx ON ONLY events (ts, id_desc);"
            ),
            "partitioned index segment must emit ON ONLY parent placeholder; got: {nidx}"
        );
    }

    #[test]
    fn partitioned_prep_emits_self_fk_shadow_columns_and_multi_pair_trigger() {
        // Regression guard: the partitioned prep emitter previously emitted
        // ONLY the parent's `id_desc` shadow column and a single-pair
        // `(id, id_desc)` autofill trigger, even when the partitioned parent
        // had a self-FK. Result: cutover later tried to RENAME a `<col>_desc`
        // shadow column that prep never created, and the autofill
        // trigger never populated the self-FK shadow value, so the
        // cutover RENAME produced NULLs in the renamed column.
        //
        // The fix mirrors the non-partitioned `emit_preparation_with_mode`
        // parent block: build a `self_pairs` vec containing
        // `(id, id_desc)` plus one entry per self-FK column, emit
        // ADD COLUMN for each self-FK shadow, and render ONE
        // multi-pair trigger covering every (src, dst) pair.
        let mut group = synth_group_single_table();
        group.parent_table = "nodes".to_string();
        group.self_fk = Some(PkFlipSelfFk {
            fk_columns: vec!["parent_id".to_string()],
            fk_constraint_names: vec!["nodes_parent_id_fkey".to_string()],
            fk_deferrable: vec![false],
            fk_initially_deferred: vec![false],
        });
        group.partitioned_parent = Some(super::super::diff::PkFlipPartitionedMeta {
            partition: PartitionSchema::Range {
                column: "ts".to_string(),
            },
        });
        let part = group.partitioned_parent.as_ref().expect("part meta");
        let prep = emit_partitioned_preparation(&group, part);
        let n = whitespace_normalize(&prep.up);
        assert!(
            n.contains("ALTER TABLE nodes ADD COLUMN id_desc bigint;"),
            "expected parent PK shadow ADD COLUMN; got: {n}"
        );
        assert!(
            n.contains("ALTER TABLE nodes ADD COLUMN parent_id_desc bigint;"),
            "expected self-FK shadow ADD COLUMN; got: {n}"
        );
        // Multi-pair trigger body must populate BOTH shadows.
        assert!(
            n.contains("heerid_to_desc(NEW.id)"),
            "expected multi-pair trigger arm for id; got: {n}"
        );
        assert!(
            n.contains("heerid_to_desc(NEW.parent_id)"),
            "expected multi-pair trigger arm for parent_id; got: {n}"
        );
        let nd = whitespace_normalize(&prep.down);
        assert!(
            nd.contains("ALTER TABLE nodes DROP COLUMN IF EXISTS id_desc;"),
            "down side missing parent shadow drop; got: {nd}"
        );
        assert!(
            nd.contains("ALTER TABLE nodes DROP COLUMN IF EXISTS parent_id_desc;"),
            "down side missing self-FK shadow drop; got: {nd}"
        );
    }

    #[test]
    fn partitioned_self_fk_index_uses_on_only_not_concurrently_on_parent() {
        // Regression guard: when the parent is partitioned and has a self-FK,
        // the self-FK shadow column index must use the partitioned-parent path
        // (`CREATE INDEX <idx> ON ONLY <parent> (<col>_desc)` plus per-leaf
        // `CONCURRENTLY` + `ATTACH PARTITION` at apply time). Prior approach
        // `CREATE INDEX CONCURRENTLY idx_<parent>_<col>_desc
        //  ON <parent> (<col>_desc)` directly against the partitioned
        // parent, which Postgres rejects with
        // "cannot create index on partitioned table … concurrently".
        let mut group = synth_group_single_table();
        group.parent_table = "events".to_string();
        group.self_fk = Some(PkFlipSelfFk {
            fk_columns: vec!["origin_event_id".to_string()],
            fk_constraint_names: vec!["events_origin_event_id_fkey".to_string()],
            fk_deferrable: vec![false],
            fk_initially_deferred: vec![false],
        });
        group.partitioned_parent = Some(super::super::diff::PkFlipPartitionedMeta {
            partition: PartitionSchema::Range {
                column: "ts".to_string(),
            },
        });
        let segments = build_segments(&group).expect("build_segments");
        // Index segment is at position 3 ([prep, backfill, verify, index, ...]).
        let index_segment = &segments[3];
        let index_bodies: Vec<&str> = index_segment
            .statements
            .iter()
            .map(|s| s.up.as_str())
            .collect();
        let joined = index_bodies.join("\n");
        let n = whitespace_normalize(&joined);
        // Must NOT directly target the partitioned parent with CONCURRENTLY.
        assert!(
            !n.contains("CREATE INDEX CONCURRENTLY idx_events_origin_event_id_desc ON events"),
            "self-FK index must not run CONCURRENTLY on partitioned parent; got: {n}"
        );
        // Must use the ON ONLY partitioned-parent form.
        assert!(
            n.contains(
                "CREATE INDEX idx_events_origin_event_id_desc ON ONLY events (origin_event_id_desc);"
            ),
            "self-FK index must use ON ONLY parent form; got: {n}"
        );
        // Must carry the partitioned self-FK label so the runner
        // expands to per-leaf CONCURRENTLY + ATTACH at apply time.
        let labels: Vec<&str> = index_segment
            .statements
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with("PkFlipPartitionedSelfFkIndex events")),
            "expected PkFlipPartitionedSelfFkIndex label; got labels: {labels:?}"
        );
    }

    // ── Reverse direction ────────────────────────────────────────────

    #[test]
    fn reverse_direction_sql_uses_to_asc_and_next() {
        let group = PkTypeFlipGroup {
            parent_table: "tbl".to_string(),
            parent_from: PkKindSchema::HeerIdRecencyBiased,
            parent_to: PkKindSchema::HeerId,
            direction: PkFlipDirection::DescToAsc,
            children: Vec::new(),
            self_fk: None,
            join_tables: Vec::new(),
            cycles: Vec::new(),
            partitioned_parent: None,
            co_destructive: false,
            co_lossy: false,
            join_table_option: crate::migrate::diff::PkFlipJoinTableOption::OptionA,
        };
        let prep = emit_preparation(&group);
        let np = whitespace_normalize(&prep.up);
        // Reverse direction substitutes heerid_to_asc in the trigger.
        assert!(np.contains("heerid_to_asc(NEW.id)"));
        let cut = emit_cutover(&group);
        let nc = whitespace_normalize(&cut.up);
        assert!(nc.contains("ALTER TABLE tbl ALTER COLUMN id_desc SET DEFAULT heerid_next();"));
    }

    // ── Determinism ──────────────────────────────────────────────────

    #[test]
    fn lower_pk_flip_group_is_byte_stable() {
        let group = synth_group_single_table();
        let plan_a = lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let plan_b = lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        assert_eq!(plan_a, plan_b);
    }

    // ── End-to-end synth via diff_bucket_maps ───────────────────────

    #[test]
    fn end_to_end_diff_to_plan_emits_six_segments_with_verification() {
        // Single-table flip emits SIX segments:
        //   1. preparation (Transactional)
        //   2. backfill CALL(s) (NonTransactional)
        //   3. verification halt point (Transactional — runner
        //      intercepts each `PkFlipVerify` statement as a count-
        //      assert; halts on non-zero count with
        //      RunnerError::PkFlipVerificationFailed)
        //   4. concurrent UNIQUE INDEX (NonTransactional)
        //   5. NOT NULL proof (Transactional)
        //   6. cutover (Transactional — POINT OF NO RETURN)
        let mut before = empty_schema();
        before.models.insert(
            "authors".to_string(),
            parent_table("authors", PkKindSchema::HeerId),
        );
        let mut after = empty_schema();
        after.models.insert(
            "authors".to_string(),
            parent_table("authors", PkKindSchema::HeerIdRecencyBiased),
        );
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after)).expect("differ");
        let group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group present");
        let plan = lower_pk_flip_group(group, bucket()).expect("lower pk flip group");
        assert_eq!(
            plan.segments.len(),
            6,
            "single-table flip emits 6 segments (with verification); got {}",
            plan.segments.len()
        );
        assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
        assert_eq!(plan.segments[1].kind, SegmentKind::NonTransactional);
        assert_eq!(plan.segments[2].kind, SegmentKind::Transactional); // verify
        assert_eq!(plan.segments[3].kind, SegmentKind::NonTransactional);
        assert_eq!(plan.segments[4].kind, SegmentKind::Transactional);
        assert_eq!(plan.segments[5].kind, SegmentKind::Transactional);
        // Verification segment statements all carry `PkFlipVerify`
        // labels.
        for stmt in &plan.segments[2].statements {
            assert!(
                stmt.label.starts_with("PkFlipVerify "),
                "verify segment label: {}",
                stmt.label
            );
        }
        // Cutover lossy marker is the point of no return.
        let cut = &plan.segments[5].statements[0];
        let warn = cut.lossy.as_ref().expect("cutover lossy");
        assert_eq!(warn.kind, LossyRollbackKind::PkTypeFlipPostCutover);
    }

    fn _silence_classification_unused() {
        let _ = Classification::PkTypeFlip {
            co_destructive: false,
            co_lossy: false,
        };
        let _ = SchemaDelta {
            bucket: bucket(),
            operations: Vec::new(),
            classification: Classification::NoOp,
        };
    }

    // ── B-5: whole-plan whitespace-normalised byte-equality ──────────
    //
    // These regressions concatenate every statement in every segment
    // of the lowered plan, normalise whitespace (collapse runs of
    // ASCII whitespace to a single space, drop empty lines, trim),
    // and assert the result equals a contractual fixture. The
    // fixtures live inline as `&str` constants so a wording / SQL
    // shape change shows up as a loud `assert_eq!` mismatch with the
    // diff embedded in the test output. Operators or reviewers
    // changing the emitter MUST update the matching fixture in the
    // same commit.
    //
    // Whitespace normalisation rule (no regex):
    //   - Walk bytes left-to-right.
    //   - Replace any run of ASCII whitespace (space, tab, CR, LF)
    //     with exactly one space.
    //   - Trim the result.
    //
    // The helper `whitespace_normalize` defined above implements the
    // rule.

    /// Concatenate every segment's `up` SQL with newline separators,
    /// then normalise whitespace. Used as the input side of every
    /// byte-equality regression below.
    fn whole_plan_normalised(plan: &MigrationPlan) -> String {
        let mut combined = String::new();
        for seg in &plan.segments {
            for stmt in &seg.statements {
                combined.push_str(&stmt.up);
                combined.push('\n');
            }
        }
        whitespace_normalize(&combined)
    }

    /// Build a single-table forward (Asc → Desc) plan rooted at
    /// table `tbl`. Used by the §3 regressions and reused as the
    /// base case for §4–§7 fixtures.
    fn lowered_plan_section_3() -> MigrationPlan {
        let group = synth_group_single_table();
        super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group")
    }

    /// Forward-direction §3 fixture — **emitter-output drift
    /// detector**. The fixture is the
    /// whitespace-normalised SQL the planner currently emits for a
    /// single-table HeerId asc → desc flip, NOT a verbatim copy of
    /// the playbook prose at `HeeRanjID-reference/docs/migrations/
    /// asc-to-desc.md` §3. The byte-equality regression below
    /// catches emitter drift across emitter changes; the structural
    /// anchor tests further down assert the fixture carries each
    /// load-bearing playbook substring (e.g. `CALL
    /// heeranjid_bulk_backfill`, `CREATE UNIQUE INDEX
    /// CONCURRENTLY`, `ALTER TABLE ... ALTER COLUMN ... SET NOT
    /// NULL`) so emitter divergence from the playbook is caught
    /// even when the new emitter output happens to byte-match the
    /// fixture (which would be impossible — the emitter generated
    /// the fixture in the first place).
    ///
    /// **Provenance.** Generated by the dumper helper below from
    /// the current emitter; the playbook §3 (lines 75–193 of
    /// asc-to-desc.md) covers the SAME logical recipe but with
    /// human prose, code-block formatting, and a worked-example
    /// table named `tbl` — the fixture matches the playbook's
    /// statement set semantically, not byte-for-byte.
    ///
    /// **Drift detector — playbook side.** A separate test below
    /// (`fixture_section_3_carries_every_playbook_anchor_substring`)
    /// walks the fixture and asserts the presence of every
    /// load-bearing playbook statement substring. If the playbook
    /// adds or removes a step, that test must be updated.
    ///
    /// **Drift detector — emitter side.** The byte-equality test
    /// (`whole_plan_byte_equality_section_3_forward`) asserts the
    /// emitter's output equals the fixture exactly. Any emitter
    /// change without a paired fixture update fails loud here.
    const EMITTER_OUTPUT_SECTION_3_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_3.sql");

    // ── Fixture regeneration helper ──────────────────────────────────
    //
    // Run with `DJOGI_DUMP_PK_FLIP_FIXTURES=1 cargo test -p djogi
    // --lib pk_flip::tests::dump_pk_flip_fixtures` to overwrite every
    // playbook fixture with the current emitter's output. Off by
    // default so the regression tests above stay deterministic.
    //
    // **WARNING — PLAYBOOK DRIFT.** The dumper writes the EMITTER's
    // output, NOT the playbook's. After re-dumping, the operator
    // MUST re-read `asc-to-desc.md` for the section in question and
    // verify the new fixture contents preserve every playbook
    // load-bearing statement substring. The companion tests
    // `fixture_section_3_carries_every_playbook_anchor_substring`
    // (and its siblings for §4 / §6 / §7 / §8 / §9) act as a
    // second-side drift detector — any deletion of a load-bearing
    // playbook substring from the fixture fails that test. Run
    // BOTH the byte-equality tests AND the anchor-substring tests
    // after dumping.
    #[test]
    fn dump_pk_flip_fixtures() {
        if std::env::var("DJOGI_DUMP_PK_FLIP_FIXTURES").ok().as_deref() != Some("1") {
            return;
        }
        let out_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/migrate/fixtures");
        std::fs::create_dir_all(&out_dir).unwrap();
        let write = |name: &str, body: String| {
            let path = out_dir.join(name);
            // Persist with a trailing newline for tidy diffs.
            let mut s = body;
            if !s.ends_with('\n') {
                s.push('\n');
            }
            std::fs::write(path, s).unwrap();
        };

        // §3 forward.
        let plan_3 = lowered_plan_section_3();
        write(
            "pk_flip_emitter_output_section_3.sql",
            whole_plan_normalised(&plan_3),
        );

        // §3 reverse.
        let mut g3r = synth_group_single_table();
        g3r.parent_from = PkKindSchema::HeerIdRecencyBiased;
        g3r.parent_to = PkKindSchema::HeerId;
        g3r.direction = PkFlipDirection::DescToAsc;
        let plan_3r = super::lower_pk_flip_group(&g3r, bucket()).expect("lower pk flip group");
        write(
            "pk_flip_emitter_output_section_3_reverse.sql",
            whole_plan_normalised(&plan_3r),
        );

        // §4 parent + child.
        let mut g4 = synth_group_single_table();
        g4.parent_table = "parent".to_string();
        g4.children.push(PkFlipChild {
            table: "c".to_string(),
            fk_column: "p_id".to_string(),
            fk_constraint_name: "c_p_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            fk_deferrable: false,
            fk_initially_deferred: false,
            fk_nullable: false,
            fk_unique: false,
            family: PkFlipFamily::Heer,
            cycle_flag: false,
        });
        let plan_4 = super::lower_pk_flip_group(&g4, bucket()).expect("lower pk flip group");
        write(
            "pk_flip_emitter_output_section_4.sql",
            whole_plan_normalised(&plan_4),
        );

        // §6 self-FK.
        let mut g6 = synth_group_single_table();
        g6.parent_table = "nodes".to_string();
        g6.self_fk = Some(PkFlipSelfFk {
            fk_columns: vec!["parent_id".to_string()],
            fk_constraint_names: vec!["nodes_parent_id_fkey".to_string()],
            fk_deferrable: vec![false],
            fk_initially_deferred: vec![false],
        });
        let plan_6 = super::lower_pk_flip_group(&g6, bucket()).expect("lower pk flip group");
        write(
            "pk_flip_emitter_output_section_6.sql",
            whole_plan_normalised(&plan_6),
        );

        // §7 join.
        let mut g7 = synth_group_single_table();
        g7.parent_table = "tags".to_string();
        g7.join_tables.push(PkFlipJoinTable {
            table: "book_tags".to_string(),
            fk_to_parent_column: "tag_id".to_string(),
            fk_to_parent_constraint: "book_tags_tag_id_fkey".to_string(),
            fk_to_parent_deferrable: false,
            fk_to_parent_initially_deferred: false,
            fk_to_partner_column: None,
            fk_to_partner_constraint: None,
            fk_to_partner_table: None,
            fk_to_partner_deferrable: false,
            fk_to_partner_initially_deferred: false,
            family: PkFlipFamily::Heer,
        });
        let plan_7 = super::lower_pk_flip_group(&g7, bucket()).expect("lower pk flip group");
        write(
            "pk_flip_emitter_output_section_7.sql",
            whole_plan_normalised(&plan_7),
        );

        // §8 cycle. B-13: cycle peer must be a first-class child
        // (`cycle_flag = true`) so the segment plan creates the
        // shadow column / trigger / index / FK on `b.a_id_desc`.
        // The fixture dumper now mirrors what the real differ
        // produces.
        let mut g8 = synth_group_single_table();
        g8.parent_table = "a".to_string();
        g8.children.push(PkFlipChild {
            table: "b".to_string(),
            fk_column: "a_id".to_string(),
            fk_constraint_name: "b_a_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            // B-16: cycle peer carries deferrable + initially_deferred.
            fk_deferrable: true,
            fk_initially_deferred: true,
            fk_nullable: true,
            fk_unique: false,
            family: PkFlipFamily::Heer,
            cycle_flag: true,
        });
        g8.cycles.push(PkFlipCycle {
            peer_table: "b".to_string(),
            peer_fk_column: "a_id".to_string(),
            self_fk_column: "b_id".to_string(),
        });
        let plan_8 = super::lower_pk_flip_group(&g8, bucket()).expect("lower pk flip group");
        write(
            "pk_flip_emitter_output_section_8.sql",
            whole_plan_normalised(&plan_8),
        );

        // §9 partitioned.
        let mut g9 = synth_group_single_table();
        g9.parent_table = "events".to_string();
        g9.partitioned_parent = Some(super::super::diff::PkFlipPartitionedMeta {
            partition: PartitionSchema::Range {
                column: "ts".to_string(),
            },
        });
        let plan_9 = super::lower_pk_flip_group(&g9, bucket()).expect("lower pk flip group");
        write(
            "pk_flip_emitter_output_section_9.sql",
            whole_plan_normalised(&plan_9),
        );
    }

    #[test]
    fn whole_plan_byte_equality_section_3_forward() {
        let plan = lowered_plan_section_3();
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_3_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §3 forward output drifted from fixture; \
             update tests/fixtures/pk_flip_emitter_output_section_3.sql or fix emitter",
        );
    }

    /// Reverse-direction §3 fixture. The hand-rolled DO block lives
    /// here verbatim so any future `heeranjid_bulk_backfill_to_asc`
    /// substrate addition must update both.
    const EMITTER_OUTPUT_SECTION_3_REVERSE_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_3_reverse.sql");

    #[test]
    fn whole_plan_byte_equality_section_3_reverse() {
        let mut group = synth_group_single_table();
        group.parent_from = PkKindSchema::HeerIdRecencyBiased;
        group.parent_to = PkKindSchema::HeerId;
        group.direction = PkFlipDirection::DescToAsc;
        let plan = super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_3_REVERSE_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §3 reverse output drifted from fixture; \
             update tests/fixtures/pk_flip_emitter_output_section_3_reverse.sql or fix emitter",
        );
    }

    /// §4 parent + child fixture (forward direction).
    const EMITTER_OUTPUT_SECTION_4_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_4.sql");

    #[test]
    fn whole_plan_byte_equality_section_4_parent_child() {
        let mut group = synth_group_single_table();
        group.parent_table = "parent".to_string();
        group.children.push(PkFlipChild {
            table: "c".to_string(),
            fk_column: "p_id".to_string(),
            fk_constraint_name: "c_p_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            fk_deferrable: false,
            fk_initially_deferred: false,
            fk_nullable: false,
            fk_unique: false,
            family: PkFlipFamily::Heer,
            cycle_flag: false,
        });
        let plan = super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_4_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §4 output drifted from fixture; update fixture or fix emitter",
        );
    }

    /// §6 self-FK fixture (forward direction).
    const EMITTER_OUTPUT_SECTION_6_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_6.sql");

    #[test]
    fn whole_plan_byte_equality_section_6_self_fk() {
        let mut group = synth_group_single_table();
        group.parent_table = "nodes".to_string();
        group.self_fk = Some(PkFlipSelfFk {
            fk_columns: vec!["parent_id".to_string()],
            fk_constraint_names: vec!["nodes_parent_id_fkey".to_string()],
            fk_deferrable: vec![false],
            fk_initially_deferred: vec![false],
        });
        let plan = super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_6_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §6 output drifted from fixture; update fixture or fix emitter",
        );
    }

    /// §7 join-table fixture (forward direction).
    const EMITTER_OUTPUT_SECTION_7_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_7.sql");

    #[test]
    fn whole_plan_byte_equality_section_7_join_table() {
        let mut group = synth_group_single_table();
        group.parent_table = "tags".to_string();
        group.join_tables.push(PkFlipJoinTable {
            table: "book_tags".to_string(),
            fk_to_parent_column: "tag_id".to_string(),
            fk_to_parent_constraint: "book_tags_tag_id_fkey".to_string(),
            fk_to_parent_deferrable: false,
            fk_to_parent_initially_deferred: false,
            fk_to_partner_column: None,
            fk_to_partner_constraint: None,
            fk_to_partner_table: None,
            fk_to_partner_deferrable: false,
            fk_to_partner_initially_deferred: false,
            family: PkFlipFamily::Heer,
        });
        let plan = super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_7_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §7 output drifted from fixture; update fixture or fix emitter",
        );
    }

    /// §8 cycle fixture (forward direction).
    const EMITTER_OUTPUT_SECTION_8_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_8.sql");

    #[test]
    fn whole_plan_byte_equality_section_8_cycle() {
        let mut group = synth_group_single_table();
        group.parent_table = "a".to_string();
        group.children.push(PkFlipChild {
            table: "b".to_string(),
            fk_column: "a_id".to_string(),
            fk_constraint_name: "b_a_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            // Cycle peers force deferrable + initially_deferred; the
            // differ does this in `promote_pk_flips_to_groups` and
            // synth fixtures must match the production shape.
            fk_deferrable: true,
            fk_initially_deferred: true,
            fk_nullable: true,
            fk_unique: false,
            family: PkFlipFamily::Heer,
            // B-13: real differ output marks cycle peers as
            // first-class children with cycle_flag = true.
            cycle_flag: true,
        });
        group.cycles.push(PkFlipCycle {
            peer_table: "b".to_string(),
            peer_fk_column: "a_id".to_string(),
            self_fk_column: "b_id".to_string(),
        });
        let plan = super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_8_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §8 output drifted from fixture; update fixture or fix emitter",
        );
    }

    /// §9 partitioned fixture (forward direction). The fixture
    /// captures the placeholder-bearing emitter output (operators
    /// applying through the runner see the placeholder expanded at
    /// apply time via pg_inherits — see runner B-2).
    const EMITTER_OUTPUT_SECTION_9_NORMALIZED: &str =
        include_str!("fixtures/pk_flip_emitter_output_section_9.sql");

    #[test]
    fn whole_plan_byte_equality_section_9_partitioned() {
        let mut group = synth_group_single_table();
        group.parent_table = "events".to_string();
        group.partitioned_parent = Some(super::super::diff::PkFlipPartitionedMeta {
            partition: PartitionSchema::Range {
                column: "ts".to_string(),
            },
        });
        let plan = super::lower_pk_flip_group(&group, bucket()).expect("lower pk flip group");
        let actual = whole_plan_normalised(&plan);
        let expected = whitespace_normalize(EMITTER_OUTPUT_SECTION_9_NORMALIZED);
        assert_eq!(
            actual, expected,
            "whole-plan §9 output drifted from fixture; update fixture or fix emitter",
        );
    }

    // ── B-5r playbook-anchor drift detectors ─────────────────────────────
    //
    // These tests walk each fixture's bytes and assert that every
    // load-bearing playbook statement is present. They catch fixture
    // drift FROM the playbook independently of the emitter — if a
    // future contributor re-runs `dump_pk_flip_fixtures` after a
    // breaking emitter change, the byte-equality test passes (because
    // emitter and fixture are now in sync) but THESE tests fail
    // (because the fixture lost a playbook-required statement).
    //
    // The fixtures are anchored to:
    //   `HeeRanjID-reference/docs/migrations/asc-to-desc.md`
    // §3 (lines 75–193), §4 (196–256), §6 (269–323), §7 (327–353),
    // §8 (356–379), §9 (383–492).

    /// §3 single-table forward — every playbook statement from §3.1
    /// through §3.6 must be present in the fixture.
    #[test]
    fn fixture_section_3_carries_every_playbook_anchor_substring() {
        // Whitespace-normalise the fixture so substring matches are
        // robust against formatting changes (multi-space → single
        // space, newlines stripped).
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_3_NORMALIZED);
        // §3.1 — ADD COLUMN.
        assert!(
            fx.contains("ALTER TABLE tbl ADD COLUMN id_desc bigint"),
            "§3.1 ADD COLUMN missing from fixture",
        );
        // §3.1 — autofill trigger function + trigger attach.
        assert!(
            fx.contains("zzz_tbl_autofill_desc"),
            "§3.1 autofill trigger name missing",
        );
        assert!(
            fx.contains("heerid_to_desc(NEW.id)"),
            "§3.1 trigger body must call heerid_to_desc on NEW.id",
        );
        assert!(
            fx.contains("BEFORE INSERT OR UPDATE ON tbl"),
            "§3.1 trigger must be BEFORE INSERT OR UPDATE",
        );
        // §3.2 — bulk backfill CALL.
        assert!(
            fx.contains("CALL heeranjid_bulk_backfill('tbl', 'id', 'id_desc', 'heer', 10000)"),
            "§3.2 bulk backfill CALL missing or signature drifted",
        );
        // §3.3 — verification SELECT.
        assert!(
            fx.contains("SELECT count(*) FROM tbl WHERE id_desc IS NULL"),
            "§3.3 NULL-shadow verification missing",
        );
        // §3.4 — concurrent unique index.
        assert!(
            fx.contains("CREATE UNIQUE INDEX CONCURRENTLY idx_tbl_id_desc ON tbl (id_desc)"),
            "§3.4 concurrent unique index missing",
        );
        // §3.5 — NOT NULL proof (4-statement non-blocking pattern).
        assert!(
            fx.contains("ADD CONSTRAINT tbl_id_desc_nn CHECK (id_desc IS NOT NULL) NOT VALID"),
            "§3.5 NOT VALID CHECK missing",
        );
        assert!(
            fx.contains("VALIDATE CONSTRAINT tbl_id_desc_nn"),
            "§3.5 VALIDATE CONSTRAINT missing",
        );
        assert!(
            fx.contains("ALTER COLUMN id_desc SET NOT NULL"),
            "§3.5 SET NOT NULL missing",
        );
        assert!(
            fx.contains("DROP CONSTRAINT tbl_id_desc_nn"),
            "§3.5 DROP CONSTRAINT (NN cleanup) missing",
        );
        // §3.6 — atomic cutover statements.
        assert!(
            fx.contains("ALTER TABLE tbl DROP CONSTRAINT tbl_pkey"),
            "§3.6 DROP old PK missing",
        );
        assert!(
            fx.contains("ADD CONSTRAINT tbl_pkey PRIMARY KEY USING INDEX idx_tbl_id_desc"),
            "§3.6 promote shadow to PK USING INDEX missing",
        );
        assert!(
            fx.contains("ALTER COLUMN id_desc SET DEFAULT heerid_next_desc()"),
            "§3.6 set new default missing",
        );
        assert!(
            fx.contains("ALTER TABLE tbl DROP COLUMN id"),
            "§3.6 drop old column missing",
        );
        assert!(
            fx.contains("DROP TRIGGER zzz_tbl_autofill_desc ON tbl"),
            "§3.6 drop trigger missing",
        );
        assert!(
            fx.contains("DROP FUNCTION zzz_tbl_autofill_desc() CASCADE"),
            "§3.6 drop trigger function missing",
        );
        assert!(
            fx.contains("ALTER TABLE tbl RENAME COLUMN id_desc TO id"),
            "§3.6 final rename missing",
        );
    }

    /// §3 reverse — substitution rule: `heerid_to_desc` →
    /// `heerid_to_asc`, `heerid_next_desc` → `heerid_next` (the
    /// asc-side default) — applied to the §3 forward fixture.
    #[test]
    fn fixture_section_3_reverse_applies_documented_substitution() {
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_3_REVERSE_NORMALIZED);
        // The reverse direction substitutes the trigger body to use
        // `heerid_to_asc` (NOT `heerid_to_desc`).
        assert!(
            fx.contains("heerid_to_asc(NEW.id)"),
            "reverse fixture must call heerid_to_asc in trigger body",
        );
        // The new column DEFAULT after cutover must be `heerid_next()`
        // — the asc generator. The fixture must NOT contain
        // `heerid_next_desc` outside an inline-comment block.
        assert!(
            fx.contains("SET DEFAULT heerid_next()"),
            "reverse fixture must set DEFAULT to heerid_next() (asc generator)",
        );
        assert!(
            !fx.contains("heerid_next_desc()"),
            "reverse fixture must NOT contain heerid_next_desc() — that is the forward default",
        );
        // Same structural shape as forward — every §3.1–§3.6 step
        // present.
        assert!(fx.contains("ALTER TABLE tbl ADD COLUMN id_desc bigint"));
        assert!(fx.contains("CREATE UNIQUE INDEX CONCURRENTLY idx_tbl_id_desc"));
        assert!(fx.contains("ALTER TABLE tbl DROP CONSTRAINT tbl_pkey"));
    }

    /// §4 parent + child — every playbook statement for the worked
    /// example with parent=`parent` and child=`c`.
    #[test]
    fn fixture_section_4_carries_every_playbook_anchor_substring() {
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_4_NORMALIZED);
        // Parent section (§3 statements applied to `parent`).
        assert!(fx.contains("ALTER TABLE parent ADD COLUMN id_desc bigint"));
        assert!(fx.contains("zzz_parent_autofill_desc"));
        // Child preparation: shadow column on c, NOT VALID FK.
        assert!(fx.contains("ALTER TABLE c ADD COLUMN p_id_desc bigint"));
        assert!(fx.contains(
            "ADD CONSTRAINT c_p_id_desc_fkey FOREIGN KEY (p_id_desc) REFERENCES parent(id_desc) NOT VALID"
        ));
        // Child backfill + validate.
        assert!(
            fx.contains("CALL heeranjid_bulk_backfill('c', 'p_id', 'p_id_desc', 'heer', 10000)")
        );
        assert!(fx.contains("VALIDATE CONSTRAINT c_p_id_desc_fkey"));
        // Cutover ordering — child FK drop happens BEFORE parent
        // promotion in the cutover transaction.
        assert!(fx.contains("ALTER TABLE c DROP CONSTRAINT c_p_id_fkey"));
        assert!(fx.contains("ALTER TABLE parent DROP CONSTRAINT parent_pkey"));
        // Parent promotion + rename.
        assert!(
            fx.contains("ADD CONSTRAINT parent_pkey PRIMARY KEY USING INDEX idx_parent_id_desc")
        );
        assert!(fx.contains("ALTER TABLE parent RENAME COLUMN id_desc TO id"));
        // Child finalisation (drop old col, rename shadow, re-add FK).
        assert!(fx.contains("ALTER TABLE c DROP COLUMN p_id"));
        assert!(fx.contains("ALTER TABLE c RENAME COLUMN p_id_desc TO p_id"));
        assert!(fx.contains("ADD CONSTRAINT c_p_id_fkey FOREIGN KEY (p_id) REFERENCES parent(id)"));
    }

    /// §6 self-FK — multi-pair trigger + cutover with FK re-creation.
    #[test]
    fn fixture_section_6_carries_every_playbook_anchor_substring() {
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_6_NORMALIZED);
        // Two shadow columns on the same table.
        assert!(fx.contains("ALTER TABLE nodes ADD COLUMN id_desc bigint"));
        assert!(fx.contains("ALTER TABLE nodes ADD COLUMN parent_id_desc bigint"));
        // Multi-pair trigger.
        assert!(fx.contains("zzz_nodes_autofill_desc"));
        // Self-FK NOT VALID.
        assert!(fx.contains(
            "ADD CONSTRAINT nodes_parent_id_desc_fkey FOREIGN KEY (parent_id_desc) REFERENCES nodes(id_desc) NOT VALID"
        ));
        // Both backfills.
        assert!(
            fx.contains("CALL heeranjid_bulk_backfill('nodes', 'id', 'id_desc', 'heer', 10000)")
        );
        assert!(fx.contains(
            "CALL heeranjid_bulk_backfill('nodes', 'parent_id', 'parent_id_desc', 'heer', 10000)"
        ));
        // Two indexes — UNIQUE on PK shadow, plain on FK shadow.
        assert!(fx.contains("CREATE UNIQUE INDEX CONCURRENTLY idx_nodes_id_desc"));
        assert!(fx.contains("CREATE INDEX CONCURRENTLY idx_nodes_parent_id_desc"));
        // Cutover: drop self-FK, drop PK, promote, drop both old
        // columns, rename both, re-add self-FK.
        assert!(fx.contains("ALTER TABLE nodes DROP CONSTRAINT nodes_parent_id_fkey"));
        assert!(fx.contains("ALTER TABLE nodes DROP CONSTRAINT nodes_pkey"));
        assert!(fx.contains("ADD CONSTRAINT nodes_pkey PRIMARY KEY USING INDEX idx_nodes_id_desc"));
        assert!(fx.contains("ALTER TABLE nodes DROP COLUMN id"));
        assert!(fx.contains("ALTER TABLE nodes DROP COLUMN parent_id"));
        assert!(fx.contains("ALTER TABLE nodes RENAME COLUMN id_desc TO id"));
        assert!(fx.contains("ALTER TABLE nodes RENAME COLUMN parent_id_desc TO parent_id"));
        assert!(fx.contains(
            "ADD CONSTRAINT nodes_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES nodes(id)"
        ));
    }

    /// §7 join table — preparation + cutover for the worked
    /// example tags/book_tags.
    #[test]
    fn fixture_section_7_carries_every_playbook_anchor_substring() {
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_7_NORMALIZED);
        // Parent shadow + trigger.
        assert!(fx.contains("ALTER TABLE tags ADD COLUMN id_desc bigint"));
        assert!(fx.contains("zzz_tags_autofill_desc"));
        // Join-table shadow + trigger + NOT VALID FK.
        assert!(fx.contains("ALTER TABLE book_tags ADD COLUMN tag_id_desc bigint"));
        assert!(fx.contains("zzz_book_tags_autofill_desc"));
        assert!(fx.contains(
            "ADD CONSTRAINT book_tags_tag_id_desc_fkey FOREIGN KEY (tag_id_desc) REFERENCES tags(id_desc) NOT VALID"
        ));
        // Both backfills.
        assert!(
            fx.contains("CALL heeranjid_bulk_backfill('tags', 'id', 'id_desc', 'heer', 10000)")
        );
        assert!(fx.contains(
            "CALL heeranjid_bulk_backfill('book_tags', 'tag_id', 'tag_id_desc', 'heer', 10000)"
        ));
        // Cutover.
        assert!(fx.contains("ALTER TABLE book_tags DROP CONSTRAINT book_tags_tag_id_fkey"));
        assert!(fx.contains("ALTER TABLE tags DROP CONSTRAINT tags_pkey"));
        assert!(fx.contains("ALTER TABLE book_tags DROP COLUMN tag_id"));
        assert!(fx.contains("ALTER TABLE book_tags RENAME COLUMN tag_id_desc TO tag_id"));
        assert!(fx.contains(
            "ADD CONSTRAINT book_tags_tag_id_fkey FOREIGN KEY (tag_id) REFERENCES tags(id)"
        ));
    }

    /// §8 cycle — DEFERRABLE FKs + SET CONSTRAINTS ALL DEFERRED.
    #[test]
    fn fixture_section_8_carries_every_playbook_anchor_substring() {
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_8_NORMALIZED);
        // The cutover MUST begin with SET CONSTRAINTS ALL DEFERRED
        // when cycles are present. (The fixture's whitespace-
        // normalised form puts everything on one line, so a
        // substring match is sufficient.)
        assert!(
            fx.contains("SET CONSTRAINTS ALL DEFERRED"),
            "§8 cycle cutover must defer all constraints",
        );
        // Cycle peer's NOT VALID FK should be DEFERRABLE INITIALLY
        // DEFERRED. The emitter renders the deferrable mode based
        // on the cycle metadata; assert the marker appears.
        assert!(
            fx.contains("DEFERRABLE INITIALLY DEFERRED"),
            "§8 cycle FK must be DEFERRABLE INITIALLY DEFERRED",
        );
    }

    /// §9 partitioned — partitioned-parent specifics: parent-level
    /// shadow, parent-level UNIQUE placeholder, ATTACH PARTITION
    /// expansion via runner, ADD PRIMARY KEY (NOT USING INDEX).
    #[test]
    fn fixture_section_9_carries_every_playbook_anchor_substring() {
        let fx = whitespace_normalize(EMITTER_OUTPUT_SECTION_9_NORMALIZED);
        // Parent-level shadow column.
        assert!(fx.contains("ALTER TABLE events ADD COLUMN id_desc bigint"));
        // Parent-level UNIQUE placeholder via ON ONLY (key §9.5
        // requirement — partitioned UNIQUE indexes can't be built
        // CONCURRENTLY at the parent level).
        assert!(
            fx.contains("ON ONLY events"),
            "§9.5 parent-level UNIQUE must be ON ONLY parent",
        );
        // Per-leaf placeholder + ATTACH expansion is done by the
        // runner at apply time; the fixture's plan SQL should
        // contain the placeholder marker `EACH_LEAF_TABLE`.
        assert!(
            fx.contains("EACH_LEAF_TABLE"),
            "§9 per-leaf placeholder marker must be present",
        );
        // §9.7: partitioned-parent PK promotion uses ADD PRIMARY KEY
        // (NOT `PRIMARY KEY USING INDEX`).
        assert!(
            fx.contains("ADD PRIMARY KEY (ts, id_desc)"),
            "§9.7 partitioned-parent PK must use ADD PRIMARY KEY (partition_key, id_desc)",
        );
        assert!(
            !fx.contains("PRIMARY KEY USING INDEX idx_events"),
            "§9.7 partitioned-parent must NOT use USING INDEX (illegal on partitioned parent)",
        );
    }

    // ── Canonical playbook fixtures ──────────────────────────────────
    //
    // Fixtures cover the EMITTER (`pk_flip_emitter_output_section_*.sql`)
    // plus anchor-substring tests vs the playbook. Separate verbatim-canonical
    // fixtures lock the PLAYBOOK against silent edits. This test set
    // closes that gap.
    //
    // **Provenance.** Each `playbook_canonical_section_*.sql`
    // fixture is a verbatim copy of the corresponding SQL code
    // fence in `HeeRanjID-reference/docs/migrations/asc-to-desc.md`
    // — the cutover block where the playbook section has one
    // (sections 3, 4, 6, 9), or the load-bearing FK creation
    // block where it doesn't (section 8's deferrable cycle FK
    // pair). Section 7 is intentionally absent — its SQL content
    // is a Rust `install_autofill_trigger_for_table` invocation,
    // not a SQL block, so there's nothing to lock at the
    // SQL-byte level.
    //
    // **Two-sided invariant.** The earlier
    // `fixture_section_*_carries_every_playbook_anchor_substring`
    // tests catch the case where the playbook prose changes a
    // statement and the fixture falls behind (the anchor walks
    // load-bearing substrings). These canonical-fixture tests
    // catch the case where the playbook AND the fixture both
    // change in lock-step but the stored fixture text drifts
    // from what the playbook actually says — a regression that
    // would slip past the substring anchors because both sides
    // agree on the (wrong) text.
    //
    // **Path probe + skip.** The playbook .md file is NOT
    // distributed with the djogi crate — it lives in the sibling
    // `HeeRanjID-reference` repo (or at the workspace root via a
    // symlink, per the project memory rule). When the test runs
    // in an environment without that file, the test skips
    // gracefully with a clear `eprintln!` rather than failing —
    // CI environments that haven't set up the symlink should
    // not block the build, but the test is informative when the
    // playbook IS present (typical local dev flow).

    /// Resolve the path to `asc-to-desc.md` relative to
    /// `CARGO_MANIFEST_DIR` (the djogi crate). Tries two well-
    /// known locations:
    ///
    ///   1. `<workspace>/HeeRanjID-reference/docs/migrations/asc-to-desc.md`
    ///      — the symlink at the project root (per project
    ///      memory rule).
    ///   2. `<workspace>/../HeeRanjID/docs/migrations/asc-to-desc.md`
    ///      — the sibling-workspace layout (per CLAUDE.md
    ///      "Workspace Layout" section).
    ///
    /// Returns `None` if neither path resolves to an existing
    /// file. Caller skips the test with a clear message.
    fn locate_playbook_md() -> Option<std::path::PathBuf> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // djogi/Cargo.toml → workspace root is one up.
        let workspace = manifest.parent()?;
        let candidates = [
            workspace.join("HeeRanjID-reference/docs/migrations/asc-to-desc.md"),
            workspace
                .parent()?
                .join("HeeRanjID/docs/migrations/asc-to-desc.md"),
        ];
        candidates.into_iter().find(|p| p.is_file())
    }

    /// Locate the SQL block at `start_line .. end_line` in the
    /// playbook .md file (1-based inclusive line numbers). The
    /// test fixture must equal this excerpt byte-for-byte.
    fn extract_playbook_lines(md: &str, start_line: usize, end_line: usize) -> String {
        md.lines()
            .skip(start_line - 1)
            .take(end_line - start_line + 1)
            .collect::<Vec<_>>()
            .join("\n")
            // Trailing newline matches the fixture file's UNIX
            // line ending convention (every `cat` of an SQL
            // fixture appends one).
            + "\n"
    }

    /// Run a single canonical-fixture byte-equality check. The
    /// fixture is `include_str!`-d at compile time so the test
    /// cannot drift independently of the binary; the playbook
    /// is `read_to_string`-d at runtime so the test catches
    /// playbook edits without recompilation.
    fn assert_canonical_section(
        section_label: &str,
        playbook: &str,
        start_line: usize,
        end_line: usize,
        fixture_bytes: &str,
    ) {
        let excerpt = extract_playbook_lines(playbook, start_line, end_line);
        if excerpt != fixture_bytes {
            // Render the divergence inline. The body is short
            // enough that printing both sides keeps the operator
            // signal readable.
            panic!(
                "Canonical fixture for {section_label} \
                 drifted from playbook text (lines {start_line}..={end_line}).\n\
                 Either the playbook edited a load-bearing SQL block — update\n\
                 the matching `playbook_canonical_section_*.sql` fixture to\n\
                 the new excerpt — or the fixture itself drifted, in which\n\
                 case revert the fixture.\n\n\
                 ---- playbook excerpt ({start_line}..={end_line}) ----\n{excerpt}\n\
                 ---- fixture ----\n{fixture_bytes}",
            );
        }
    }

    #[test]
    fn playbook_canonical_section_3_locks_against_silent_edits() {
        let Some(md_path) = locate_playbook_md() else {
            eprintln!(
                "[skip] B-5r canonical fixture §3 — playbook asc-to-desc.md not\n\
                 reachable from djogi/. Set up the HeeRanjID-reference symlink at\n\
                 the workspace root (or the sibling HeeRanjID workspace) to\n\
                 enable this lock.",
            );
            return;
        };
        let md = std::fs::read_to_string(&md_path).expect("read playbook");
        // §3.6 cutover code fence body — lines 178–187 inclusive.
        // Trailing newline appended by `extract_playbook_lines`
        // matches the fixture file's UNIX EOF convention.
        assert_canonical_section(
            "§3 cutover",
            &md,
            178,
            187,
            include_str!("fixtures/playbook_canonical_section_3.sql"),
        );
    }

    #[test]
    fn playbook_canonical_section_4_locks_against_silent_edits() {
        let Some(md_path) = locate_playbook_md() else {
            eprintln!("[skip] B-5r §4 — see §3 skip note for setup");
            return;
        };
        let md = std::fs::read_to_string(&md_path).expect("read playbook");
        // §4 cutover code fence body — lines 232–254 inclusive.
        assert_canonical_section(
            "§4 cutover",
            &md,
            232,
            254,
            include_str!("fixtures/playbook_canonical_section_4.sql"),
        );
    }

    #[test]
    fn playbook_canonical_section_6_locks_against_silent_edits() {
        let Some(md_path) = locate_playbook_md() else {
            eprintln!("[skip] B-5r §6 — see §3 skip note for setup");
            return;
        };
        let md = std::fs::read_to_string(&md_path).expect("read playbook");
        // §6 cutover code fence body — lines 307–322 inclusive.
        assert_canonical_section(
            "§6 cutover",
            &md,
            307,
            322,
            include_str!("fixtures/playbook_canonical_section_6.sql"),
        );
    }

    #[test]
    fn playbook_canonical_section_8_locks_against_silent_edits() {
        let Some(md_path) = locate_playbook_md() else {
            eprintln!("[skip] B-5r §8 — see §3 skip note for setup");
            return;
        };
        let md = std::fs::read_to_string(&md_path).expect("read playbook");
        // §8 deferred FK creation code fence body — lines
        // 362–372 inclusive. Section 8 has no cutover SQL block
        // (the cycle case piggy-backs on §3.6 / §4 with the
        // SET CONSTRAINTS ALL DEFERRED prologue described in
        // prose).
        assert_canonical_section(
            "§8 deferrable FK pair",
            &md,
            362,
            372,
            include_str!("fixtures/playbook_canonical_section_8.sql"),
        );
    }

    #[test]
    fn playbook_canonical_section_9_locks_against_silent_edits() {
        let Some(md_path) = locate_playbook_md() else {
            eprintln!("[skip] B-5r §9 — see §3 skip note for setup");
            return;
        };
        let md = std::fs::read_to_string(&md_path).expect("read playbook");
        // §9.7 partitioned-parent cutover code fence body —
        // lines 459–477 inclusive.
        assert_canonical_section(
            "§9 partitioned cutover",
            &md,
            459,
            477,
            include_str!("fixtures/playbook_canonical_section_9.sql"),
        );
    }
}
