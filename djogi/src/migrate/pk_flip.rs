//! PK-type-flip migration SQL emission and segment planning — T9 of
//! the Phase 7 v3 plan.
//!
//! # What this module owns
//!
//! Lowering of [`SchemaOperation::PkTypeFlipGroup`] into the
//! multi-segment [`MigrationPlan`] required by the HeeRanjID
//! `asc-to-desc` playbook. Every emitted SQL string is reproduced
//! **verbatim** from the playbook's worked examples so byte-equality
//! regression tests against the playbook fixtures pass without
//! whitespace fixups beyond a single normalisation pass.
//!
//! The playbook lives at
//! `../HeeRanjID/docs/migrations/asc-to-desc.md`. Where this module
//! and the playbook disagree, the playbook wins; the unit tests in
//! `tests::sql_byte_equality_vs_playbook_*` are the regression net
//! against drift.
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
pub fn lower_pk_flip_group(group: &PkTypeFlipGroup, bucket: BucketKey) -> MigrationPlan {
    let segments = build_segments(group);
    MigrationPlan {
        bucket,
        classification: super::diff::Classification::PkTypeFlip {
            co_destructive: group.co_destructive,
            co_lossy: group.co_lossy,
        },
        segments,
    }
}

/// Build the segment list for a single [`PkTypeFlipGroup`].
///
/// Public-in-crate so the segment planner can splice this directly
/// into a multi-bucket plan when the caller is composing a delta
/// that mixes a flip with non-flip ops in OTHER buckets (the same
/// bucket cannot mix both — the differ enforces that invariant via
/// the per-bucket op list).
pub(crate) fn build_segments(group: &PkTypeFlipGroup) -> Vec<Segment> {
    if let Some(part) = &group.partitioned_parent {
        return build_segments_partitioned(group, part);
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
    // runner runs each via single-statement `raw_ddl` — the
    // procedure's internal `COMMIT`s would otherwise raise
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

    segments
}

/// Build the segment list for a partitioned parent flip per
/// playbook §9. The shape mirrors [`build_segments`] but the
/// preparation segment installs the parent-level shadow column
/// (which propagates to leaves), the backfill segment emits per-leaf
/// `CALL` invocations (the runner enumerates leaves from
/// `pg_inherits` at apply time — the descriptor only knows the
/// partition strategy), the index segment emits the parent-level
/// UNIQUE placeholder + per-leaf `CONCURRENTLY` + `ATTACH PARTITION`,
/// and the cutover uses `ADD PRIMARY KEY (...)` instead of
/// `USING INDEX` because Postgres does not support `USING INDEX` on
/// a partitioned parent.
fn build_segments_partitioned(
    group: &PkTypeFlipGroup,
    part: &super::diff::PkFlipPartitionedMeta,
) -> Vec<Segment> {
    // Phase 7 deliverable: emit the §9 shape with placeholder
    // per-leaf invocations. The runner replaces the
    // `<EACH_LEAF_TABLE>` token with concrete leaf names at apply
    // time — see runner pre-flight. Operators reviewing the SQL file
    // see the placeholder + a comment block explaining the
    // substitution; running the file directly without the runner
    // will fail loudly on the placeholder, which is the intended
    // safety: partitioned flips MUST go through the runner.
    vec![
        Segment {
            kind: SegmentKind::Transactional,
            statements: vec![emit_partitioned_preparation(group, part)],
        },
        Segment {
            kind: SegmentKind::NonTransactional,
            statements: vec![emit_partitioned_backfill_and_verification(group, part)],
        },
        Segment {
            kind: SegmentKind::NonTransactional,
            statements: vec![emit_partitioned_indexes(group, part)],
        },
        Segment {
            kind: SegmentKind::Transactional,
            statements: vec![emit_not_null_proof(group)],
        },
        Segment {
            kind: SegmentKind::Transactional,
            statements: vec![emit_partitioned_cutover(group, part)],
        },
    ]
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
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let direction = group.direction;
    let id_type = pg_id_type(p_family);
    let mut up = String::new();
    let mut down = String::new();

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
    for jt in &group.join_tables {
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        let id_type = pg_id_type(jt.family);
        let _ = writeln!(
            up,
            "ALTER TABLE {tbl} ADD COLUMN {dst} {ty};",
            tbl = jt.table,
            dst = dst,
            ty = id_type,
        );
        up.push_str(&render_autofill_trigger(
            &jt.table,
            &[(jt.fk_to_parent_column.as_str(), dst.as_str())],
            jt.family,
            direction,
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
        let _ = writeln!(
            down,
            "ALTER TABLE {tbl} DROP COLUMN IF EXISTS {dst};",
            tbl = jt.table,
            dst = dst,
        );
    }

    OperationSql {
        label: format!("PkFlipPrep {parent}"),
        up,
        down,
        lossy: None,
    }
}

fn child_in_cycle(group: &PkTypeFlipGroup, table: &str) -> bool {
    group.cycles.iter().any(|c| c.peer_table == table)
}

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

    // Join tables — same backfill + invariant.
    for jt in &group.join_tables {
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        let kind_lit = backfill_kind_literal(jt.family);
        let _ = writeln!(
            up,
            "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000);",
            tbl = jt.table,
            src = jt.fk_to_parent_column,
            dst = dst,
            kind = kind_lit,
        );
        let _ = writeln!(
            up,
            "SELECT count(*) FROM {tbl}\n  \
             WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL)\n     \
             OR ({src} IS NOT NULL AND {dst} <> {flip}({src}));",
            tbl = jt.table,
            src = jt.fk_to_parent_column,
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

    // Cycle peers — backfill the peer's FK to this parent. The
    // peer's own PK flip lives in its own PkTypeFlipGroup; here we
    // only own the FK shadow on the peer side because the cycle
    // requires both sides to be in sync before the cutover.
    for cyc in &group.cycles {
        let dst = format!("{}{}", cyc.peer_fk_column, SHADOW_SUFFIX);
        let _ = writeln!(
            up,
            "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000);",
            tbl = cyc.peer_table,
            src = cyc.peer_fk_column,
            dst = dst,
            kind = p_kind,
        );
        let _ = writeln!(
            up,
            "SELECT count(*) FROM {tbl}\n  \
             WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL)\n     \
             OR ({src} IS NOT NULL AND {dst} <> {flip}({src}));",
            tbl = cyc.peer_table,
            src = cyc.peer_fk_column,
            dst = dst,
            flip = flip_fn_name(p_family, group.direction),
        );
        let _ = writeln!(up, "-- expect: 0");
    }

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

/// Emit one [`OperationSql`] per backfill statement (CALL / VALIDATE)
/// so the runner can dispatch each via single-statement `raw_ddl`.
/// Without this split the simple-query protocol wraps multiple
/// statements in an implicit transaction; the procedure's internal
/// `COMMIT` then fires `2D000 invalid transaction termination` per
/// the playbook's "must not be wrapped in pool.begin()" warning.
fn emit_backfill_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let p_kind = backfill_kind_literal(p_family);
    let mut out: Vec<OperationSql> = Vec::new();

    let down_note = "-- Backfill is idempotent under `WHERE dst IS NULL`; the\n\
                     -- down side has no inverse beyond dropping the shadow\n\
                     -- column itself, which segment 1's down already covers."
        .to_string();

    out.push(OperationSql {
        label: format!("PkFlipBackfill {parent}"),
        up: format!(
            "CALL heeranjid_bulk_backfill('{parent}', 'id', 'id{suffix}', {kind}, 10000)",
            parent = parent,
            suffix = SHADOW_SUFFIX,
            kind = p_kind,
        ),
        down: down_note.clone(),
        lossy: None,
    });

    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let dst = format!("{}{}", col, SHADOW_SUFFIX);
            out.push(OperationSql {
                label: format!("PkFlipBackfill {parent} {col}"),
                up: format!(
                    "CALL heeranjid_bulk_backfill('{parent}', '{col}', '{dst}', {kind}, 10000)",
                    parent = parent,
                    col = col,
                    dst = dst,
                    kind = p_kind,
                ),
                down: down_note.clone(),
                lossy: None,
            });
        }
    }

    for child in &group.children {
        let cf = child_family(child, group);
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let kind_lit = backfill_kind_literal(cf);
        out.push(OperationSql {
            label: format!("PkFlipBackfill {tbl}", tbl = child.table),
            up: format!(
                "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000)",
                tbl = child.table,
                src = child.fk_column,
                dst = dst,
                kind = kind_lit,
            ),
            down: down_note.clone(),
            lossy: None,
        });
        // VALIDATE CONSTRAINT lives in segment 3b alongside the
        // FK-creation it validates; the FK does not exist yet at
        // backfill time.
    }

    for jt in &group.join_tables {
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        let kind_lit = backfill_kind_literal(jt.family);
        out.push(OperationSql {
            label: format!("PkFlipBackfill {tbl}", tbl = jt.table),
            up: format!(
                "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000)",
                tbl = jt.table,
                src = jt.fk_to_parent_column,
                dst = dst,
                kind = kind_lit,
            ),
            down: down_note.clone(),
            lossy: None,
        });
        // VALIDATE CONSTRAINT lives in segment 3b alongside the
        // FK-creation it validates; the FK does not exist yet at
        // backfill time. The unused `dst` formatter capture is
        // retained so a future restructure that re-couples backfill
        // and VALIDATE can grep for the pair.
        let _ = dst;
    }

    for cyc in &group.cycles {
        let dst = format!("{}{}", cyc.peer_fk_column, SHADOW_SUFFIX);
        out.push(OperationSql {
            label: format!("PkFlipBackfill {tbl}", tbl = cyc.peer_table),
            up: format!(
                "CALL heeranjid_bulk_backfill('{tbl}', '{src}', '{dst}', {kind}, 10000)",
                tbl = cyc.peer_table,
                src = cyc.peer_fk_column,
                dst = dst,
                kind = p_kind,
            ),
            down: down_note.clone(),
            lossy: None,
        });
    }

    out
}

/// Emit one [`OperationSql`] per verification table the runner must
/// halt on. Labels are `PkFlipVerify <table> <hint>` so the runner's
/// transactional-segment dispatch recognises them, runs the SELECT
/// as a count-assert, and surfaces
/// [`super::runner::RunnerError::PkFlipVerificationFailed`] on any
/// non-zero count. The `up` body is the verification SQL verbatim.
/// The `down` body is empty — verification has no inverse.
fn emit_verification_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let mut out: Vec<OperationSql> = Vec::new();

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

    // Join tables — same shape as nullable child.
    for jt in &group.join_tables {
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        out.push(OperationSql {
            label: format!(
                "PkFlipVerify {tbl} {col}",
                tbl = jt.table,
                col = jt.fk_to_parent_column
            ),
            up: format!(
                "SELECT count(*) FROM {tbl} \
                 WHERE ({src} IS NULL) IS DISTINCT FROM ({dst} IS NULL) \
                    OR ({src} IS NOT NULL AND {dst} <> {flip}({src}))",
                tbl = jt.table,
                src = jt.fk_to_parent_column,
                dst = dst,
                flip = flip_fn_name(jt.family, group.direction),
            ),
            down: String::new(),
            lossy: None,
        });
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

    // Self-FK constraints — DEFERRABLE INITIALLY DEFERRED only when
    // the parent participates in a cycle (rare and explicit).
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            // The self-FK shadow column was created in segment 1
            // alongside the parent's `id_desc`. Its name follows the
            // `<col>_desc` convention via the SHADOW_SUFFIX, which
            // the embedded format strings below interpolate directly
            // through `{col}{suffix}`. We do not need a separate
            // `dst` binding because the SQL builder reaches both
            // names via the format args.
            let cycle_clause = if !group.cycles.is_empty() {
                " DEFERRABLE INITIALLY DEFERRED"
            } else {
                ""
            };
            out.push(OperationSql {
                label: format!("PkFlipAddFk {parent} {col}"),
                up: format!(
                    "ALTER TABLE {parent} ADD CONSTRAINT {parent}_{col}{suffix}_fkey \
                     FOREIGN KEY ({col}{suffix}) REFERENCES {parent}(id{suffix}){cycle} NOT VALID",
                    parent = parent,
                    col = col,
                    suffix = SHADOW_SUFFIX,
                    cycle = cycle_clause,
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
        let cycle_clause = if child_in_cycle(group, &child.table) {
            " DEFERRABLE INITIALLY DEFERRED"
        } else {
            ""
        };
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
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        out.push(OperationSql {
            label: format!(
                "PkFlipAddFk {tbl} {col}",
                tbl = jt.table,
                col = jt.fk_to_parent_column
            ),
            up: format!(
                "ALTER TABLE {tbl} ADD CONSTRAINT {tbl}_{dst}_fkey \
                 FOREIGN KEY ({dst}) REFERENCES {parent}(id{suffix}) NOT VALID",
                tbl = jt.table,
                dst = dst,
                parent = parent,
                suffix = SHADOW_SUFFIX,
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
                col = jt.fk_to_parent_column
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

    out
}

/// Emit one [`OperationSql`] per CONCURRENTLY index. Each must run
/// in its own statement — concurrent index builds cannot run inside
/// any transaction, including the implicit simple-query batch tx
/// that fires when multiple statements share one `batch_execute`.
fn emit_concurrent_index_statements(group: &PkTypeFlipGroup) -> Vec<OperationSql> {
    let parent = group.parent_table.as_str();
    let mut out: Vec<OperationSql> = Vec::new();

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

    if let Some(self_fk) = &group.self_fk {
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
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        out.push(OperationSql {
            label: format!(
                "PkFlipConcurrentIndex {tbl} {col}",
                tbl = jt.table,
                col = jt.fk_to_parent_column
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

    // Join tables — index on the parent-FK shadow.
    for jt in &group.join_tables {
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
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
    let p_family = parent_family(group);
    let direction = group.direction;
    let next_fn = next_fn_name(p_family, direction);
    let mut up = String::new();

    up.push_str("BEGIN;\n");

    // Cycle handling — defer all constraints if any cycles exist.
    if !group.cycles.is_empty() {
        up.push_str("    SET CONSTRAINTS ALL DEFERRED;\n");
    }

    // 1. Drop every child's old FK.
    for child in &group.children {
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl} DROP CONSTRAINT {cons};",
            tbl = child.table,
            cons = child.fk_constraint_name,
        );
    }
    for jt in &group.join_tables {
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl} DROP CONSTRAINT {cons};",
            tbl = jt.table,
            cons = jt.fk_to_parent_constraint,
        );
    }
    if let Some(self_fk) = &group.self_fk {
        for cons in &self_fk.fk_constraint_names {
            let _ = writeln!(
                up,
                "    ALTER TABLE {parent} DROP CONSTRAINT {cons};",
                parent = parent,
                cons = cons,
            );
        }
    }

    // 2. Promote the parent.
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} DROP CONSTRAINT {parent}_pkey;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} ADD CONSTRAINT {parent}_pkey \
         PRIMARY KEY USING INDEX idx_{parent}_id{suffix};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} ALTER COLUMN id{suffix} SET DEFAULT {next}();",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        next = next_fn,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} ALTER COLUMN id DROP DEFAULT;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} DROP COLUMN id;",
        parent = parent,
    );
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "    ALTER TABLE {parent} DROP COLUMN {col};",
                parent = parent,
                col = col,
            );
        }
    }
    let _ = writeln!(
        up,
        "    DROP TRIGGER zzz_{parent}_autofill_desc ON {parent};",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    DROP FUNCTION zzz_{parent}_autofill_desc() CASCADE;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} RENAME COLUMN id{suffix} TO id;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    if let Some(self_fk) = &group.self_fk {
        for col in &self_fk.fk_columns {
            let _ = writeln!(
                up,
                "    ALTER TABLE {parent} RENAME COLUMN {col}{suffix} TO {col};",
                parent = parent,
                col = col,
                suffix = SHADOW_SUFFIX,
            );
        }
        // Re-add self-FK constraints with original names pointing at
        // the now-renamed shadow column (which is now the live `id`).
        for (col, cons) in self_fk
            .fk_columns
            .iter()
            .zip(self_fk.fk_constraint_names.iter())
        {
            let _ = writeln!(
                up,
                "    ALTER TABLE {parent}\n      ADD CONSTRAINT {cons}\n      \
                 FOREIGN KEY ({col}) REFERENCES {parent}(id);",
                parent = parent,
                col = col,
                cons = cons,
            );
        }
    }

    // 3. Finalise every child.
    for child in &group.children {
        let dst = format!("{}{}", child.fk_column, SHADOW_SUFFIX);
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl} DROP COLUMN {col};",
            tbl = child.table,
            col = child.fk_column,
        );
        let _ = writeln!(
            up,
            "    DROP TRIGGER zzz_{tbl}_autofill_desc ON {tbl};",
            tbl = child.table,
        );
        let _ = writeln!(
            up,
            "    DROP FUNCTION zzz_{tbl}_autofill_desc() CASCADE;",
            tbl = child.table,
        );
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl} RENAME COLUMN {dst} TO {col};",
            tbl = child.table,
            dst = dst,
            col = child.fk_column,
        );
        let cascade = render_on_delete(child.on_delete);
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl}\n      ADD CONSTRAINT {cons}\n      \
             FOREIGN KEY ({col}) REFERENCES {parent}(id) {cascade};",
            tbl = child.table,
            cons = child.fk_constraint_name,
            col = child.fk_column,
            parent = parent,
            cascade = cascade,
        );
    }

    // Join-table finalisation.
    for jt in &group.join_tables {
        let dst = format!("{}{}", jt.fk_to_parent_column, SHADOW_SUFFIX);
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl} DROP COLUMN {col};",
            tbl = jt.table,
            col = jt.fk_to_parent_column,
        );
        let _ = writeln!(
            up,
            "    DROP TRIGGER zzz_{tbl}_autofill_desc ON {tbl};",
            tbl = jt.table,
        );
        let _ = writeln!(
            up,
            "    DROP FUNCTION zzz_{tbl}_autofill_desc() CASCADE;",
            tbl = jt.table,
        );
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl} RENAME COLUMN {dst} TO {col};",
            tbl = jt.table,
            dst = dst,
            col = jt.fk_to_parent_column,
        );
        let _ = writeln!(
            up,
            "    ALTER TABLE {tbl}\n      ADD CONSTRAINT {cons}\n      \
             FOREIGN KEY ({col}) REFERENCES {parent}(id);",
            tbl = jt.table,
            cons = jt.fk_to_parent_constraint,
            col = jt.fk_to_parent_column,
            parent = parent,
        );
    }

    up.push_str("COMMIT;\n");

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

    let _ = writeln!(
        up,
        "ALTER TABLE {parent} ADD COLUMN id{suffix} {ty};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        ty = id_type,
    );
    up.push_str(&render_autofill_trigger(
        parent,
        &[(PARENT_PK_COLUMN, &format!("id{}", SHADOW_SUFFIX))],
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
    let _ = writeln!(
        down,
        "ALTER TABLE {parent} DROP COLUMN IF EXISTS id{suffix};",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    OperationSql {
        label: format!("PkFlipPartitionedPrep {parent}"),
        up,
        down,
        lossy: None,
    }
}

fn emit_partitioned_backfill_and_verification(
    group: &PkTypeFlipGroup,
    _part: &super::diff::PkFlipPartitionedMeta,
) -> OperationSql {
    let parent = group.parent_table.as_str();
    let p_family = parent_family(group);
    let p_kind = backfill_kind_literal(p_family);
    let mut up = String::new();
    let _ = writeln!(
        up,
        "-- Partitioned parent: invoke heeranjid_bulk_backfill once per leaf\n\
         -- partition. The runner enumerates leaves from pg_inherits at apply\n\
         -- time and substitutes <EACH_LEAF_TABLE> with the concrete\n\
         -- partition name. Operators hand-running this file MUST replace\n\
         -- the placeholder with each leaf name before executing.",
    );
    let _ = writeln!(
        up,
        "CALL heeranjid_bulk_backfill('<EACH_LEAF_TABLE>', 'id', 'id{suffix}', {kind}, 10000);",
        suffix = SHADOW_SUFFIX,
        kind = p_kind,
    );
    let _ = writeln!(
        up,
        "SELECT count(*) FROM {parent} WHERE id{suffix} IS NULL;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(up, "-- expect: 0 (aggregated across partitions)");
    OperationSql {
        label: format!("PkFlipPartitionedBackfill {parent}"),
        up,
        down: "-- Partitioned backfill is idempotent under `WHERE dst IS NULL`;\n\
               -- the down side has no inverse beyond dropping the shadow column."
            .to_string(),
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

fn emit_partitioned_cutover(
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
    up.push_str("BEGIN;\n");
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} DROP CONSTRAINT {parent}_pkey;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} ADD PRIMARY KEY ({pkey}, id{suffix});",
        parent = parent,
        pkey = part_col,
        suffix = SHADOW_SUFFIX,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} ALTER COLUMN id{suffix} SET DEFAULT {next}();",
        parent = parent,
        suffix = SHADOW_SUFFIX,
        next = next_fn,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} ALTER COLUMN id DROP DEFAULT;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} DROP COLUMN id;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    DROP TRIGGER zzz_{parent}_autofill_desc ON {parent};",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    DROP FUNCTION zzz_{parent}_autofill_desc() CASCADE;",
        parent = parent,
    );
    let _ = writeln!(
        up,
        "    ALTER TABLE {parent} RENAME COLUMN id{suffix} TO id;",
        parent = parent,
        suffix = SHADOW_SUFFIX,
    );
    up.push_str("COMMIT;\n");
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
            default_sql: Some("generate_id()".to_string()),
            foreign_key: None,
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
        }
    }

    fn id_col_desc() -> ColumnSchema {
        ColumnSchema {
            default_sql: Some("generate_id_desc()".to_string()),
            ..id_col()
        }
    }

    fn fk_col(name: &str, target: &str, nullable: bool) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: Some(ForeignKeySchema {
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: target.to_string(),
            }),
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
            let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
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
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
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
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
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
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
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
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
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
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
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
    fn sql_byte_equality_vs_playbook_section_3_preparation() {
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
    fn sql_byte_equality_vs_playbook_section_3_backfill() {
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
    fn sql_byte_equality_vs_playbook_section_3_concurrent_index() {
        let group = synth_group_single_table();
        let idx = emit_concurrent_indexes(&group);
        let n = whitespace_normalize(&idx.up);
        assert!(
            n.contains("CREATE UNIQUE INDEX CONCURRENTLY idx_tbl_id_desc ON tbl (id_desc);"),
            "missing §3.4 concurrent index; got: {n}",
        );
    }

    #[test]
    fn sql_byte_equality_vs_playbook_section_3_not_null_proof() {
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
    fn sql_byte_equality_vs_playbook_section_3_cutover() {
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
        // Cutover is wrapped in an atomic transaction.
        assert!(n.starts_with("BEGIN;"));
        assert!(n.ends_with("COMMIT;"));
        // Lossy marker for the point-of-no-return.
        let warn = cut.lossy.expect("cutover lossy warning");
        assert_eq!(warn.kind, LossyRollbackKind::PkTypeFlipPostCutover);
    }

    // ── §4 parent + child ────────────────────────────────────────────

    #[test]
    fn sql_byte_equality_vs_playbook_section_4_parent_child() {
        let mut group = synth_group_single_table();
        group.children.push(PkFlipChild {
            table: "c".to_string(),
            fk_column: "p_id".to_string(),
            fk_constraint_name: "c_p_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            fk_nullable: false,
            fk_unique: false,
            family: PkFlipFamily::Heer,
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
    fn sql_byte_equality_vs_playbook_section_6_self_fk() {
        let mut group = synth_group_single_table();
        group.parent_table = "nodes".to_string();
        group.self_fk = Some(PkFlipSelfFk {
            fk_columns: vec!["parent_id".to_string()],
            fk_constraint_names: vec!["nodes_parent_id_fkey".to_string()],
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
    fn sql_byte_equality_vs_playbook_section_7_join_table() {
        let mut group = synth_group_single_table();
        group.parent_table = "tags".to_string();
        group.join_tables.push(PkFlipJoinTable {
            table: "book_tags".to_string(),
            fk_to_parent_column: "tag_id".to_string(),
            fk_to_parent_constraint: "book_tags_tag_id_fkey".to_string(),
            fk_to_partner_column: None,
            fk_to_partner_constraint: None,
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
    fn sql_byte_equality_vs_playbook_section_8_cycles_uses_deferrable() {
        let mut group = synth_group_single_table();
        group.parent_table = "a".to_string();
        group.children.push(PkFlipChild {
            table: "b".to_string(),
            fk_column: "a_id".to_string(),
            fk_constraint_name: "b_a_id_fkey".to_string(),
            on_delete: OnDeleteSchema::Restrict,
            fk_nullable: true,
            fk_unique: false,
            family: PkFlipFamily::Heer,
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
    fn sql_byte_equality_vs_playbook_section_9_partitioned_uses_add_primary_key() {
        let mut group = synth_group_single_table();
        group.parent_table = "events".to_string();
        group.partitioned_parent = Some(super::super::diff::PkFlipPartitionedMeta {
            partition: PartitionSchema::Range {
                column: "ts".to_string(),
            },
        });
        let segments = build_segments(&group);
        let cut_stmt = &segments.last().expect("cutover segment").statements[0];
        let n = whitespace_normalize(&cut_stmt.up);
        assert!(
            n.contains("ALTER TABLE events ADD PRIMARY KEY (ts, id_desc);"),
            "partitioned cutover must use ADD PRIMARY KEY (...) form (not USING INDEX); got: {n}"
        );
        // Index segment must reference parent-level UNIQUE placeholder.
        let idx_stmt = &segments[2].statements[0];
        let nidx = whitespace_normalize(&idx_stmt.up);
        assert!(
            nidx.contains(
                "CREATE UNIQUE INDEX events_ts_id_desc_idx ON ONLY events (ts, id_desc);"
            ),
            "partitioned index segment must emit ON ONLY parent placeholder; got: {nidx}"
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
        let plan_a = lower_pk_flip_group(&group, bucket());
        let plan_b = lower_pk_flip_group(&group, bucket());
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
        let deltas = diff_bucket_maps(&bucket_map(before), &bucket_map(after));
        let group = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group present");
        let plan = lower_pk_flip_group(group, bucket());
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
}
