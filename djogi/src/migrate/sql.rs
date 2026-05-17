//! SQL emission for the Phase 7 migration engine — lowers a typed
//! [`SchemaDelta`] into reviewable Postgres `up` / `down` statement
//! pairs.
//!
//! # Scope
//!
//! T3 owns the *standard* lowering path: every [`SchemaOperation`]
//! variant the differ emits, plus the deterministic-naming and
//! quoting policy for identifiers. T3 does **not** own:
//!
//! - `PkTypeFlip` orchestration. T9 plugs in later with the full
//!   expand / contract / FK-cascade playbook. T3 surfaces every flip
//!   as [`SqlEmitError::PkTypeFlipMustRouteToT9`] so the differ can
//!   never accidentally feed a flip through the standard path.
//! - Migration file naming, checksums, ledger writes — T6 owns the
//!   on-disk shape.
//!
//! # Determinism
//!
//! Two runs of [`lower_delta`] on the same input produce
//! byte-identical output. The emitter walks owned `BTreeMap` /
//! ordered `Vec` data, never iterates a `HashMap`, and never embeds a
//! timestamp / random ID / hashed-input-dependent value into the
//! output. The only hashed value is the index-name digest from
//! [`crate::descriptor::index_name`], which is itself deterministic
//! within a process and stored in the snapshot.
//!
//! # Identifier quoting
//!
//! Every emitted identifier is double-quoted (`"users"`,
//! `"created_at"`). Identifiers reach the emitter from the
//! descriptor / projection layer, which already enforces the
//! ASCII-letter-or-underscore-then-alphanumerics rule and the 63-byte
//! limit (see [`crate::ident`]). We still quote so reserved keywords
//! that slip through a future descriptor change cannot turn into a
//! parse error inside generated SQL.
//!
//! # Lossy rollback
//!
//! `DropColumn`, `DropTable`, `DropEnum`, and `DropIndex` cannot
//! reconstruct row data on rollback. The emitter populates a
//! [`LossyRollbackWarning`] alongside each such operation so the
//! runner / operator can surface the warning at apply time. The
//! generated `down` SQL is a SQL comment that names the loss; T3
//! does not invent a recreate-table-from-thin-air statement.
//!
//! `DropIndex` is the only Drop variant whose down side rebuilds
//! cleanly — the differ carries the full [`IndexSchema`] in the
//! variant payload (per T2 fixup B-4) so the recreate is a real
//! `CREATE INDEX` statement. The lossy marker still surfaces because
//! the rebuild itself can be expensive on large tables.
//!
//! # Diff-shape notes
//!
//! Both [`SchemaOperation::AddForeignKey`] and
//! [`SchemaOperation::DropForeignKey`] now carry the full
//! [`crate::migrate::schema::ForeignKeySchema`] — target table,
//! target column, and `on_delete` cascade — so forward and rollback
//! SQL are both fully recoverable. Earlier T3 review rounds
//! (round-1 B-3) fixed an inversion where the emitter silently
//! lowered every FK as `ON DELETE RESTRICT`; round-2 A-1
//! consolidated the inline-FK path on `ForeignKeySchema.on_delete`
//! so all SQL emit sites read the same field. There is no longer a
//! lossy / hand-edit fallback for FK ops; the migration round-trips
//! cleanly.
//!
//! # No regex
//!
//! Per project rule (`feedback_no_regex_in_djogi.md`), this module
//! uses byte-level checks for every identifier / SQL-string scan.
//! There is no `regex` crate dependency anywhere in the migration
//! engine.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::diff::{
    Classification, ColumnChange, EnumVariantAnchor, EnumVariantAnchorKind, SchemaDelta,
    SchemaOperation,
};
use super::projection::BucketKey;
use super::schema::{
    ColumnSchema, EnumSchema, ExclusionConstraintSchema, ForeignKeySchema, IndexColumnSchema,
    IndexKindSchema, IndexNullsOrderSchema, IndexOrderSchema, IndexSchema, IndexTargetSchema,
    IndexTypeSchema, OnDeleteSchema, PartitionSchema, PkKindSchema, TableSchema,
};

// ── Public output shapes ──────────────────────────────────────────────────

/// One operation lowered into reviewable up / down SQL.
///
/// The `up` field always holds executable SQL. `down` always holds
/// SQL too — but for [lossy](LossyRollbackWarning) operations the
/// `down` string is a single SQL comment naming the loss, not an
/// actual recreate / restore statement. Operators see the comment in
/// the generated migration file and decide whether to hand-write a
/// rollback path. See the module-level "Lossy rollback" docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSql {
    /// One-line operator-facing label, e.g. `"AddTable users"`. Used
    /// by the runner's per-segment progress output and by error
    /// messages naming the failing operation.
    pub label: String,
    /// Forward SQL — applied on `up`.
    pub up: String,
    /// Reverse SQL — applied on `down`. May be a SQL-comment
    /// placeholder for lossy / non-invertible operations; in that
    /// case [`OperationSql::lossy`] is `Some(...)`.
    pub down: String,
    /// `Some(...)` when the down side cannot fully restore prior
    /// state. Surfaces the reason so the runner can warn loudly
    /// instead of silently recording a useless rollback.
    pub lossy: Option<LossyRollbackWarning>,
}

/// Surfaced alongside every operation whose `down` SQL cannot
/// reconstruct the original state.
///
/// `kind` discriminates the structural reason (column / table / enum
/// / index drops are the canonical non-invertibles); `detail` is a
/// human-readable note. The runner / `migrations status` surfaces
/// both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossyRollbackWarning {
    /// Structural reason for the loss.
    pub kind: LossyRollbackKind,
    /// Operator-facing detail string, e.g.
    /// `"column users.legacy_id dropped — original type and data
    ///   not recoverable from the diff"`.
    pub detail: String,
}

/// Why a rollback is lossy.
///
/// `DropForeignKey` is **not** in this enum: the diff carries the
/// full [`ForeignKeySchema`] through to
/// [`emit_drop_foreign_key`], so the rollback recreates the
/// constraint with the original target + cascade and never produces a
/// lossy marker. Codex T3 round-2 review A-2 removed an earlier
/// reserved variant that the emitter never produced — pre-publish
/// stage, no compat shim, the variant is gone. A future shape that
/// strips FK metadata before reaching the emitter would re-add the
/// variant at that point with the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossyRollbackKind {
    /// `DropColumn` — column shape and row data are gone.
    DropColumn,
    /// `DropTable` — full table is gone, including row data.
    DropTable,
    /// `DropEnum` — Postgres type is gone; values previously stored
    /// in dependent columns are gone with it (those columns are
    /// dropped first).
    DropEnum,
    /// `DropIndex` — the index definition is gone; queries that
    /// relied on the index will go back to sequential scans.
    DropIndex,
    /// PK-type-flip cutover (T9 segment 5) committed — the prior PK
    /// column, its DEFAULT, and the autofill trigger are gone.
    /// Rollback requires an inverse migration: add the previous-
    /// direction column back, install a reverse autofill trigger,
    /// re-run `heeranjid_bulk_backfill`, and run a second cutover.
    /// Surfaces in `migrations status` as the "POINT OF NO RETURN"
    /// marker.
    PkTypeFlipPostCutover,
}

/// Errors the SQL emitter surfaces.
///
/// Every variant carries enough context for an actionable operator
/// message — the lower-level layer never panics or silently drops a
/// problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlEmitError {
    /// The differ flagged a transition it could not lower safely.
    /// T3 propagates the reason verbatim; the operator hand-writes
    /// the migration.
    Unsupported {
        /// The original `Unsupported { reason }` payload from the
        /// differ.
        reason: String,
    },
    /// A `PkTypeFlip` operation reached the standard SQL path. Phase
    /// 7 owns these via T9's expand / contract orchestration; the
    /// standard path must never lower one. Fail loudly so the
    /// caller routes through T9.
    PkTypeFlipMustRouteToT9 {
        /// Postgres table name carrying the flip.
        table: String,
        /// Source kind.
        from: PkKindSchema,
        /// Target kind.
        to: PkKindSchema,
    },
    /// A descriptor-defined partition shape changed in a way the
    /// standard emitter cannot lower. Postgres has no `ALTER TABLE
    /// ... SET PARTITION BY ...`; partition method changes require
    /// a full table rebuild with operator confirmation.
    UnsupportedPartitionChange {
        /// Postgres table whose partition shape changed.
        table: String,
        /// Operator-facing detail.
        detail: String,
    },
    /// A `#[model(storage_params = "...")]` fragment failed the
    /// structural reloption grammar. These fragments are emitted
    /// unquoted inside `ALTER TABLE ... SET (...)`, so the emitter
    /// rejects malformed values instead of splicing them into SQL.
    InvalidStorageParams {
        /// Original storage-parameter fragment.
        params: String,
        /// Operator-facing validation reason.
        reason: String,
    },
    /// The differ or PK-flip lowerer rejected a cluster before SQL
    /// emission could proceed.
    Diff(super::diff::DiffError),
}

impl std::fmt::Display for SqlEmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlEmitError::Unsupported { reason } => {
                write!(f, "migration cannot be lowered automatically: {reason}")
            }
            SqlEmitError::PkTypeFlipMustRouteToT9 { table, from, to } => write!(
                f,
                "table `{table}`: PK-type flip ({from:?} -> {to:?}) reached the standard \
                 SQL emitter — these are orchestrated by T9's expand/contract playbook \
                 and must never go through the standard path"
            ),
            SqlEmitError::UnsupportedPartitionChange { table, detail } => write!(
                f,
                "table `{table}`: partition shape change cannot be lowered automatically: \
                 {detail}"
            ),
            SqlEmitError::InvalidStorageParams { params, reason } => {
                write!(f, "invalid storage_params `{params}`: {reason}")
            }
            SqlEmitError::Diff(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SqlEmitError {}

// ── Façade ────────────────────────────────────────────────────────────────

/// Lower a single bucket's [`SchemaDelta`] into a per-operation list
/// of SQL pairs.
///
/// **No-op short-circuit.** A delta classified [`Classification::NoOp`]
/// (and therefore carrying an empty `operations` vector) returns an
/// empty `Vec`. The segment planner above then emits zero segments
/// for the bucket.
///
/// **Hard-error surfaces.** [`SchemaOperation::Unsupported`] and
/// [`SchemaOperation::PkTypeFlip`] both fail this fn — they never
/// produce executable SQL on the standard path. Callers should
/// inspect the delta's [`Classification`] before lowering when they
/// want to route a flip to T9; T3's default behaviour is "fail
/// loudly so a flip cannot silently mis-apply".
///
/// `clippy::result_large_err` is silenced because [`SqlEmitError`]
/// is a structural error type whose payload size matters less than
/// callers being able to inspect every variant without boxing — the
/// projection layer takes the same stance for [`crate::migrate::
/// projection::ProjectionError`].
#[allow(clippy::result_large_err)]
pub fn lower_delta(delta: &SchemaDelta) -> Result<Vec<OperationSql>, SqlEmitError> {
    if matches!(delta.classification, Classification::NoOp) {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(delta.operations.len());
    for op in &delta.operations {
        out.push(lower_operation(op)?);
    }
    Ok(out)
}

/// Lower a single [`SchemaOperation`] into its SQL pair.
///
/// Crate-private so the segment planner can interleave lowering with
/// segment classification without re-walking the full delta. External
/// consumers go through [`lower_delta`].
#[allow(clippy::result_large_err)]
pub(crate) fn lower_operation(op: &SchemaOperation) -> Result<OperationSql, SqlEmitError> {
    match op {
        SchemaOperation::AddTable(t) => try_emit_add_table(t),
        SchemaOperation::DropTable(name) => Ok(emit_drop_table(name)),
        SchemaOperation::RenameTable { from, to } => Ok(emit_rename_table(from, to)),
        SchemaOperation::AddColumn { table, column } => Ok(emit_add_column(table, column)),
        SchemaOperation::DropColumn { table, column } => Ok(emit_drop_column(table, column)),
        SchemaOperation::RenameColumn { table, from, to } => {
            Ok(emit_rename_column(table, from, to))
        }
        SchemaOperation::AlterColumn {
            table,
            column,
            change,
        } => Ok(emit_alter_column(table, column, change)),
        SchemaOperation::AddForeignKey { table, column, fk } => {
            Ok(emit_add_foreign_key(table, column, fk))
        }
        SchemaOperation::DropForeignKey { table, column, fk } => {
            Ok(emit_drop_foreign_key(table, column, fk))
        }
        SchemaOperation::AddIndex(idx) => Ok(emit_add_index(idx)),
        SchemaOperation::DropIndex(idx) => Ok(emit_drop_index(idx)),
        SchemaOperation::AddExclusionConstraint { table, exclusion } => {
            Ok(emit_add_exclusion_constraint(table, exclusion))
        }
        SchemaOperation::DropExclusionConstraint {
            table,
            name,
            exclusion,
        } => Ok(emit_drop_exclusion_constraint(table, name, exclusion)),
        SchemaOperation::AddEnum(e) => Ok(emit_add_enum(e)),
        SchemaOperation::DropEnum(name) => Ok(emit_drop_enum(name)),
        SchemaOperation::AddEnumVariant {
            enum_name,
            variant,
            anchor,
        } => Ok(emit_add_enum_variant(enum_name, variant, anchor.as_ref())),
        // Phase 8.5 Cluster 4 djogi#217 — `COMMENT ON TABLE` lowering.
        // The composer owns setting-independent SQL string rendering
        // for the comment text.
        SchemaOperation::SetTableComment { table, from, to } => Ok(emit_set_table_comment(
            table,
            from.as_deref(),
            to.as_deref(),
        )),
        SchemaOperation::SetStorageParams { table, from, to } => {
            emit_set_storage_params(table, from.as_deref(), to.as_deref())
        }
        SchemaOperation::SetTablespace { table, from, to } => {
            Ok(emit_set_tablespace(table, from.as_deref(), to.as_deref()))
        }
        SchemaOperation::PkTypeFlip { table, from, to } => {
            Err(SqlEmitError::PkTypeFlipMustRouteToT9 {
                table: table.clone(),
                from: from.clone(),
                to: to.clone(),
            })
        }
        SchemaOperation::PkTypeFlipGroup(group) => {
            // The group's full lowering produces a multi-segment
            // plan via [`crate::migrate::pk_flip::lower_pk_flip_group`].
            // From a per-operation perspective we emit a SUMMARY
            // OperationSql whose `up` is a comment block describing
            // the contained statements; the segment planner then
            // SUPERSEDES this summary with the real multi-segment
            // plan when it sees `PkTypeFlipGroup` in the delta.
            // Direct callers of `lower_delta` (e.g. compose's SQL
            // file writer) get a comment summary that documents the
            // structure — the matching segment plan provides the
            // executable SQL.
            let mut summary = String::new();
            let _ = std::fmt::Write::write_fmt(
                &mut summary,
                format_args!(
                    "-- PkTypeFlipGroup parent={parent} {from:?} -> {to:?}\n\
                     -- children={children}, self_fk={self_fk}, join_tables={join},\n\
                     -- cycles={cycles}, partitioned={partitioned}.\n\
                     -- See the segment plan for the executable SQL (preparation,\n\
                     -- backfill, concurrent index, NOT NULL proof, cutover).",
                    parent = group.parent_table,
                    from = group.parent_from,
                    to = group.parent_to,
                    children = group.children.len(),
                    self_fk = group.self_fk.is_some(),
                    join = group.join_tables.len(),
                    cycles = group.cycles.len(),
                    partitioned = group.partitioned_parent.is_some(),
                ),
            );
            Ok(OperationSql {
                label: format!("PkTypeFlipGroup {}", group.parent_table),
                up: summary,
                down: format!(
                    "-- PkTypeFlipGroup parent={} — see cutover segment for the\n\
                     -- POINT OF NO RETURN marker; rollback requires an inverse\n\
                     -- migration.",
                    group.parent_table,
                ),
                lossy: Some(LossyRollbackWarning {
                    kind: LossyRollbackKind::PkTypeFlipPostCutover,
                    detail: format!(
                        "PkTypeFlipGroup `{}` cutover removes the prior PK column \
                         and trigger; rollback requires an inverse migration",
                        group.parent_table,
                    ),
                }),
            })
        }
        SchemaOperation::PkTypeFlipMultiGroup(groups) => {
            // Codex round-4 B-15 — the multi-parent variant. Same
            // summary shape as the single-parent group, repeated
            // per cluster member; the executable SQL lives in the
            // stage-interleaved segment plan that
            // [`crate::migrate::pk_flip::build_segments_multi`]
            // produces. Direct callers of `lower_delta` see a
            // comment block here so the migration file documents
            // the cluster's shape; the segment planner supersedes
            // this summary with the real interleaved plan when it
            // sees `PkTypeFlipMultiGroup` in the delta.
            let mut summary = String::new();
            let _ = std::fmt::Write::write_fmt(
                &mut summary,
                format_args!(
                    "-- PkTypeFlipMultiGroup parents={count}\n\
                     -- See the stage-interleaved segment plan for the executable SQL\n\
                     -- (one prep / backfill / index / FK / NOT NULL / cutover\n\
                     -- segment touching every cluster member at each stage, per\n\
                     -- HeeRanjID asc-to-desc playbook §7).\n",
                    count = groups.len(),
                ),
            );
            for g in groups {
                let _ = std::fmt::Write::write_fmt(
                    &mut summary,
                    format_args!(
                        "-- member parent={parent} {from:?} -> {to:?} children={children},\n\
                         --        self_fk={self_fk}, join_tables={join}, cycles={cycles},\n\
                         --        partitioned={partitioned}.\n",
                        parent = g.parent_table,
                        from = g.parent_from,
                        to = g.parent_to,
                        children = g.children.len(),
                        self_fk = g.self_fk.is_some(),
                        join = g.join_tables.len(),
                        cycles = g.cycles.len(),
                        partitioned = g.partitioned_parent.is_some(),
                    ),
                );
            }
            // Cluster label = comma-joined parent_table names.
            let mut label = String::from("PkTypeFlipMultiGroup ");
            let names: Vec<&str> = groups.iter().map(|g| g.parent_table.as_str()).collect();
            label.push_str(&names.join(","));
            let detail = format!(
                "PkTypeFlipMultiGroup [{names}] cutover removes prior PK columns and \
                 triggers across every cluster member; rollback requires an inverse \
                 migration",
                names = names.join(","),
            );
            Ok(OperationSql {
                label,
                up: summary,
                down: format!(
                    "-- PkTypeFlipMultiGroup [{names}] — see cutover segment for the\n\
                     -- POINT OF NO RETURN marker; rollback requires an inverse\n\
                     -- migration.",
                    names = names.join(","),
                ),
                lossy: Some(LossyRollbackWarning {
                    kind: LossyRollbackKind::PkTypeFlipPostCutover,
                    detail,
                }),
            })
        }
        SchemaOperation::RenameApp { from, to } => Ok(emit_rename_app(from, to)),
        SchemaOperation::MoveModelBetweenApps {
            model,
            from_app,
            to_app,
        } => Ok(emit_move_model_between_apps(model, from_app, to_app)),
        SchemaOperation::Unsupported { reason } => Err(SqlEmitError::Unsupported {
            reason: reason.clone(),
        }),
    }
}

/// Snapshot bucket the lowered SQL belongs to. Re-exported here so
/// downstream consumers can pair an `OperationSql` with its
/// originating bucket without reaching into `super::projection`.
pub type SqlBucket = BucketKey;

// ── Per-operation emitters ────────────────────────────────────────────────

#[cfg(test)]
fn emit_add_table(t: &TableSchema) -> OperationSql {
    try_emit_add_table(t).expect("test fixture storage_params should be valid")
}

fn try_emit_add_table(t: &TableSchema) -> Result<OperationSql, SqlEmitError> {
    let qt = quote_ident(&t.table);
    let mut up = String::with_capacity(256);
    let _ = writeln!(up, "CREATE TABLE {qt} (");
    let mut first = true;
    for col in &t.columns {
        if !first {
            up.push_str(",\n");
        }
        first = false;
        up.push_str("    ");
        write_column_definition(&mut up, col, &t.table);
    }
    // Composite / declarative PK constraint — emit at the table level
    // when the PK shape calls for it. Single-column non-Composite PKs
    // are inlined as `PRIMARY KEY` on the column itself via
    // `write_column_definition`. Composite + Custom shapes always go
    // here so the column-level definitions stay free of `PRIMARY KEY`
    // markers that would conflict with the constraint.
    if let Some(pk_clause) = pk_table_clause(t) {
        up.push_str(",\n    ");
        up.push_str(&pk_clause);
    }
    // Phase 7.5 PR 7: inline `EXCLUDE` constraints. EXCLUDE-on-
    // populated classifies as OfflineOnly, but a brand-new table
    // necessarily has zero rows — declaring the constraint inline
    // means Postgres registers it as part of the table create with no
    // separate ALTER pass. Constraint name comes from the descriptor;
    // emission order matches the projection's name-sorted slice for
    // deterministic SQL. The standalone
    // `AddExclusionConstraint` variant is reserved for "add EXCLUDE
    // to an already-existing table" which never reaches the live
    // runner (OfflineOnly verdict from the classifier).
    for exclusion in &t.exclusion_constraints {
        up.push_str(",\n    CONSTRAINT ");
        up.push_str(&quote_ident(&exclusion.name));
        up.push(' ');
        up.push_str(&render_exclusion_body(exclusion));
    }
    up.push('\n');
    up.push(')');
    if let Some(part) = &t.partition {
        up.push(' ');
        up.push_str(&partition_clause(part));
    }
    up.push(';');

    // Phase 8.5 Cluster 4 djogi#217 — append `COMMENT ON TABLE …`
    // immediately after the `CREATE TABLE` statement when the
    // adopter declared `#[model(table_comment = "…")]`. The composer
    // renders the text through a setting-independent SQL string
    // literal helper so apostrophes and backslashes round-trip safely.
    if let Some(comment) = t.table_comment.as_deref() {
        up.push('\n');
        let _ = write!(
            up,
            "COMMENT ON TABLE {qt} IS {};",
            render_comment_literal(comment)
        );
    }
    // Phase 8.5 Cluster 4 djogi#217 — append `COMMENT ON COLUMN …`
    // for every column carrying `#[field(comment = "…")]`. Emission
    // order matches column declaration order (the same order the
    // CREATE TABLE column list above uses) so the resulting migration
    // SQL is byte-stable across runs.
    for col in &t.columns {
        if let Some(comment) = col.comment.as_deref() {
            up.push('\n');
            up.push_str(&render_comment_on_column(
                &t.table,
                &col.name,
                Some(comment),
            ));
        }
    }
    if let Some(params) = t.storage_params.as_deref() {
        up.push('\n');
        up.push_str(&render_set_storage_params(&t.table, params)?);
    }
    if let Some(tablespace) = t.tablespace.as_deref() {
        up.push('\n');
        up.push_str(&render_set_tablespace(&t.table, Some(tablespace)));
    }

    let down = format!("DROP TABLE {qt};");
    Ok(OperationSql {
        label: format!("AddTable {}", t.table),
        up,
        down,
        // AddTable's down-side `DROP TABLE` is clean SQL — but the
        // *forward* operation has no data-loss concern. We mark
        // lossy=None on AddTable; the lossy marker is for the
        // forward op's reverse, and the reverse here (DROP TABLE)
        // is itself only lossy when applied — by then we are doing
        // a rollback, and the runner will flag rollbacks of CREATE
        // TABLE as data loss anyway. Keep this simple: AddTable up
        // is clean; AddTable down is "drop a freshly-created table
        // that had no rows". Not lossy.
        lossy: None,
    })
}

fn emit_drop_table(name: &str) -> OperationSql {
    let qn = quote_ident(name);
    let up = format!("DROP TABLE {qn};");
    let down = format!(
        "-- LOSSY ROLLBACK: cannot recreate table `{name}` from the diff.\n\
         -- The migration emitter has no historical schema for the dropped\n\
         -- table; rollback must be hand-written if needed."
    );
    OperationSql {
        label: format!("DropTable {name}"),
        up,
        down,
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::DropTable,
            detail: format!(
                "table `{name}` dropped — schema and rows are lost; rollback \
                 has no recreate path"
            ),
        }),
    }
}

fn emit_rename_table(from: &str, to: &str) -> OperationSql {
    let qf = quote_ident(from);
    let qt = quote_ident(to);
    OperationSql {
        label: format!("RenameTable {from} -> {to}"),
        up: format!("ALTER TABLE {qf} RENAME TO {qt};"),
        down: format!("ALTER TABLE {qt} RENAME TO {qf};"),
        lossy: None,
    }
}

fn emit_add_column(table: &str, col: &ColumnSchema) -> OperationSql {
    let qt = quote_ident(table);
    let qc = quote_ident(&col.name);
    let mut up = String::with_capacity(128);
    let _ = write!(up, "ALTER TABLE {qt} ADD COLUMN ");
    write_column_definition(&mut up, col, table);
    up.push(';');
    // Phase 8.5 Cluster 4 djogi#217 — emit `COMMENT ON COLUMN …`
    // immediately after `ADD COLUMN` when the descriptor carries a
    // comment. The differ filters back-compat snapshots that
    // load with `comment: None` so this path only fires when the
    // adopter actually declared `#[field(comment = "…")]` on the
    // new column.
    if let Some(comment) = col.comment.as_deref() {
        up.push('\n');
        up.push_str(&render_comment_on_column(table, &col.name, Some(comment)));
    }
    let down = format!("ALTER TABLE {qt} DROP COLUMN {qc};");
    OperationSql {
        label: format!("AddColumn {table}.{}", col.name),
        up,
        down,
        // The down side here drops a freshly-added column whose only
        // values are whatever the application wrote between apply
        // and rollback. Mark lossy so the runner surfaces the risk.
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::DropColumn,
            detail: format!(
                "column `{}.{}` dropped on rollback — any values written between \
                 apply and rollback are lost",
                table, col.name
            ),
        }),
    }
}

fn emit_drop_column(table: &str, column: &str) -> OperationSql {
    let qt = quote_ident(table);
    let qc = quote_ident(column);
    let up = format!("ALTER TABLE {qt} DROP COLUMN {qc};");
    let down = format!(
        "-- LOSSY ROLLBACK: cannot rebuild column `{table}.{column}` from the diff.\n\
         -- The migration emitter has no original `ColumnSchema` for the dropped\n\
         -- column; rollback must be hand-written if needed."
    );
    OperationSql {
        label: format!("DropColumn {table}.{column}"),
        up,
        down,
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::DropColumn,
            detail: format!(
                "column `{table}.{column}` dropped — original type and row data \
                 not recoverable from the diff"
            ),
        }),
    }
}

fn emit_rename_column(table: &str, from: &str, to: &str) -> OperationSql {
    let qt = quote_ident(table);
    let qf = quote_ident(from);
    let qto = quote_ident(to);
    OperationSql {
        label: format!("RenameColumn {table}.{from} -> {to}"),
        up: format!("ALTER TABLE {qt} RENAME COLUMN {qf} TO {qto};"),
        down: format!("ALTER TABLE {qt} RENAME COLUMN {qto} TO {qf};"),
        lossy: None,
    }
}

fn emit_alter_column(table: &str, column: &str, change: &ColumnChange) -> OperationSql {
    let qt = quote_ident(table);
    let qc = quote_ident(column);
    let (up, down, label_suffix, lossy) = match change {
        ColumnChange::SetNullable(true) => (
            format!("ALTER TABLE {qt} ALTER COLUMN {qc} DROP NOT NULL;"),
            format!("ALTER TABLE {qt} ALTER COLUMN {qc} SET NOT NULL;"),
            "drop NOT NULL",
            None,
        ),
        ColumnChange::SetNullable(false) => (
            format!("ALTER TABLE {qt} ALTER COLUMN {qc} SET NOT NULL;"),
            format!("ALTER TABLE {qt} ALTER COLUMN {qc} DROP NOT NULL;"),
            "set NOT NULL",
            None,
        ),
        ColumnChange::SetDefault(Some(expr)) => (
            format!("ALTER TABLE {qt} ALTER COLUMN {qc} SET DEFAULT {expr};"),
            // We don't know the previous default; a clean rollback
            // requires the operator to capture the prior value
            // before re-running. Mark non-lossy structurally
            // (rollback is just a DROP DEFAULT) but leave a comment
            // hint that the prior default is lost.
            format!(
                "-- NOTE: prior DEFAULT for `{table}.{column}` not recoverable from the diff.\n\
                 ALTER TABLE {qt} ALTER COLUMN {qc} DROP DEFAULT;"
            ),
            "set DEFAULT",
            None,
        ),
        ColumnChange::SetDefault(None) => (
            format!("ALTER TABLE {qt} ALTER COLUMN {qc} DROP DEFAULT;"),
            format!(
                "-- NOTE: prior DEFAULT for `{table}.{column}` not recoverable from the diff.\n\
                 -- Rollback would need the original DEFAULT expression."
            ),
            "drop DEFAULT",
            None,
        ),
        ColumnChange::ChangeType { from, to } => {
            // `USING <col>::<new_type>` is a sensible default cast.
            // Operators with bespoke conversions can hand-edit the
            // emitted file before apply.
            let up = format!("ALTER TABLE {qt} ALTER COLUMN {qc} TYPE {to} USING {qc}::{to};");
            let down =
                format!("ALTER TABLE {qt} ALTER COLUMN {qc} TYPE {from} USING {qc}::{from};");
            (up, down, "change TYPE", None)
        }
        ColumnChange::SetCheck { from, to } => {
            // Postgres has no `ALTER COLUMN ... SET CHECK`; check
            // constraints live at the table level. Emit a named
            // table-level constraint so DROP / ADD reach the same
            // constraint-name slot deterministically.
            //
            // The variant carries both prior (`from`) and target (`to`)
            // expressions so the down side can fully restore the
            // pre-operation CHECK state — no lossy comment placeholder.
            // GPT-5.5 review pinned the lossy-rollback gap: the previous
            // `SetCheck(Option<String>)` rendered the drop arm as DROP
            // up / comment-only down, which left the column
            // unconstrained after rollback (visible whenever a type
            // migration on a checked column was rolled back).
            let constraint = check_constraint_name(table, column);
            let qcons = quote_ident(&constraint);
            let (up, down, label) = match (from, to) {
                (Some(prior), None) => {
                    // DROP — drop the existing constraint on apply;
                    // re-add the prior expression on rollback.
                    (
                        format!("ALTER TABLE {qt} DROP CONSTRAINT {qcons};"),
                        format!("ALTER TABLE {qt} ADD CONSTRAINT {qcons} CHECK ({prior});"),
                        "drop CHECK",
                    )
                }
                (None, Some(expr)) => {
                    // ADD — install the new constraint on apply; drop
                    // it on rollback (no prior to restore).
                    (
                        format!("ALTER TABLE {qt} ADD CONSTRAINT {qcons} CHECK ({expr});"),
                        format!("ALTER TABLE {qt} DROP CONSTRAINT {qcons};"),
                        "set CHECK",
                    )
                }
                (Some(prior), Some(expr)) => {
                    // AMEND in one step — drop then re-add inside a
                    // single emitted SQL chunk. The differ currently
                    // splits AMEND across two `SetCheck` entries; this
                    // arm is the structural completion for callers
                    // that prefer the merged form.
                    (
                        format!(
                            "ALTER TABLE {qt} DROP CONSTRAINT {qcons};\n\
                             ALTER TABLE {qt} ADD CONSTRAINT {qcons} CHECK ({expr});"
                        ),
                        format!(
                            "ALTER TABLE {qt} DROP CONSTRAINT {qcons};\n\
                             ALTER TABLE {qt} ADD CONSTRAINT {qcons} CHECK ({prior});"
                        ),
                        "amend CHECK",
                    )
                }
                (None, None) => {
                    // No-op pair. The differ never produces this shape
                    // (the (None, None) and (Some(b), Some(a)) where
                    // b == a cases are filtered upstream). If it ever
                    // does, emit a no-op SQL comment rather than
                    // garbage SQL.
                    (
                        format!(
                            "-- noop SetCheck on `{table}.{column}` (from == to == None); \
                             likely a differ bug.\n"
                        ),
                        format!("-- noop SetCheck rollback on `{table}.{column}`.\n"),
                        "noop CHECK",
                    )
                }
            };
            (up, down, label, None)
        }
        ColumnChange::SetUnique(true) => {
            let constraint = unique_constraint_name(table, column);
            let qcons = quote_ident(&constraint);
            let up = format!("ALTER TABLE {qt} ADD CONSTRAINT {qcons} UNIQUE ({qc});");
            let down = format!("ALTER TABLE {qt} DROP CONSTRAINT {qcons};");
            (up, down, "set UNIQUE", None)
        }
        ColumnChange::SetUnique(false) => {
            let constraint = unique_constraint_name(table, column);
            let qcons = quote_ident(&constraint);
            let up = format!("ALTER TABLE {qt} DROP CONSTRAINT {qcons};");
            let down = format!("ALTER TABLE {qt} ADD CONSTRAINT {qcons} UNIQUE ({qc});");
            (up, down, "drop UNIQUE", None)
        }
        ColumnChange::SetIndexed(true) => {
            // Implicit per-column index — derive a deterministic
            // name. Use `index_name` from the descriptor module so
            // this matches the differ's index-naming convention if
            // and when implicit-indexed columns surface as full
            // `IndexSchema` entries instead.
            let name = crate::descriptor::index_name(
                table,
                crate::descriptor::IndexNameKind::NonUnique,
                crate::descriptor::IndexNameTarget::Columns(&[column]),
            );
            let qname = quote_ident(&name);
            let up = format!("CREATE INDEX {qname} ON {qt} ({qc});");
            let down = format!("DROP INDEX {qname};");
            (up, down, "set indexed", None)
        }
        ColumnChange::SetIndexed(false) => {
            let name = crate::descriptor::index_name(
                table,
                crate::descriptor::IndexNameKind::NonUnique,
                crate::descriptor::IndexNameTarget::Columns(&[column]),
            );
            let qname = quote_ident(&name);
            let up = format!("DROP INDEX {qname};");
            let down = format!("CREATE INDEX {qname} ON {qt} ({qc});");
            (up, down, "drop indexed", None)
        }
        // Stored generated column transitions classify as
        // OfflineOnly per the v3 plan — Postgres has no
        // `ALTER COLUMN ADD GENERATED` for stored expressions, so the
        // operator must hand-edit a DROP COLUMN + ADD COLUMN sequence.
        // We emit a comment placeholder so the migration file
        // documents the intent without producing executable SQL the
        // runner would refuse anyway. The classifier (PR 7 task 3)
        // ensures the live runner never reaches this path.
        ColumnChange::SetGenerated { from: _, to } => {
            let kind = if to.is_some() {
                "set GENERATED"
            } else {
                "drop GENERATED"
            };
            let up = format!(
                "-- OfflineOnly: stored generated column change for `{table}.{column}`\n\
                 -- has no online ALTER form. Hand-edit the migration to DROP COLUMN +\n\
                 -- ADD COLUMN with the new generation expression. See\n\
                 -- `docs/spec/decisions.md` and the `generated_column_refusal` pattern."
            );
            let down = format!(
                "-- OfflineOnly: revert requires reconstructing the original\n\
                 -- generation state on `{table}.{column}` (DROP + ADD COLUMN).\n\
                 -- Hand-edit before applying."
            );
            (up, down, kind, None)
        }
        // Codex T22 BLOCK-3: identity-column transitions emit the
        // proper `ALTER COLUMN ADD/DROP/SET GENERATED` syntax.
        //
        // Codex T22 round-3 BLOCK-2 + round-4 refinement: the None →
        // Some(kind) transition additionally emits a setval follow-up
        // to sync the new sequence to the existing row data. Without
        // this, `ADD GENERATED ... AS IDENTITY` allocates a fresh
        // sequence that starts at MIN_VALUE (default 1) regardless
        // of existing rows — the next default-id INSERT collides
        // with row 1 on a populated table.
        //
        // The setval uses the three-arg form `setval(seq, val, false)`
        // (is_called = false), which sets the sequence so the NEXT
        // call returns `val` rather than `val + 1`. This avoids the
        // round-3 bug where `setval(seq, 0)` on an empty table failed
        // with "value 0 is out of bounds for sequence ..." (default
        // minvalue=1). With is_called=false:
        //
        //   - Empty table: GREATEST(COALESCE(MAX, 0), 0) + 1 = 1.
        //     setval(seq, 1, false). Next call returns 1. ✓
        //   - Populated max=N (positive): N + 1. setval(seq, N+1, false).
        //     Next call returns N+1. ✓
        //   - Negative-ids edge case (max=-5): GREATEST(-5, 0) = 0;
        //     +1 = 1. Next call returns 1. Caller responsible for
        //     ensuring no collision with hand-set positive ids in
        //     this case (rare).
        //
        // The DROP IDENTITY down-direction still needs the setval
        // because the rollback re-adds the identity sequence — same
        // collision risk regardless of direction.
        ColumnChange::SetIdentity { from, to } => {
            let qt = quote_ident(table);
            let qc = quote_ident(column);
            match (from, to) {
                (None, Some(kind)) => {
                    let clause = kind.sql_clause();
                    let up = format!(
                        "ALTER TABLE {qt} ALTER COLUMN {qc} ADD {clause};\n\
                         SELECT setval(pg_get_serial_sequence('{table}', '{column}'), \
                         GREATEST(COALESCE((SELECT MAX({qc}) FROM {qt}), 0), 0) + 1, false);"
                    );
                    let down = format!("ALTER TABLE {qt} ALTER COLUMN {qc} DROP IDENTITY;");
                    (up, down, "add IDENTITY", None)
                }
                (Some(prev), None) => {
                    let clause = prev.sql_clause();
                    let up = format!("ALTER TABLE {qt} ALTER COLUMN {qc} DROP IDENTITY;");
                    // Down also needs to sync the sequence — adding
                    // identity back on a populated table has the same
                    // collision risk regardless of which direction.
                    let down = format!(
                        "ALTER TABLE {qt} ALTER COLUMN {qc} ADD {clause};\n\
                         SELECT setval(pg_get_serial_sequence('{table}', '{column}'), \
                         GREATEST(COALESCE((SELECT MAX({qc}) FROM {qt}), 0), 0) + 1, false);"
                    );
                    (up, down, "drop IDENTITY", None)
                }
                (Some(prev), Some(next)) => {
                    // Kind change (BY DEFAULT ↔ ALWAYS). Postgres'
                    // ALTER COLUMN SET GENERATED <kind> changes only
                    // the kind, preserving the existing sequence.
                    let next_clause = next.sql_clause();
                    let prev_clause = prev.sql_clause();
                    // Extract the GENERATED ... portion (drop the
                    // initial "GENERATED " from sql_clause to get
                    // "BY DEFAULT AS IDENTITY" / "ALWAYS AS IDENTITY").
                    // Actually Postgres syntax for kind change is
                    // SET GENERATED { ALWAYS | BY DEFAULT } — without
                    // "AS IDENTITY". So split on " AS ".
                    let next_kind_only = next_clause
                        .strip_prefix("GENERATED ")
                        .and_then(|s| s.split(" AS ").next())
                        .unwrap_or(next_clause);
                    let prev_kind_only = prev_clause
                        .strip_prefix("GENERATED ")
                        .and_then(|s| s.split(" AS ").next())
                        .unwrap_or(prev_clause);
                    let up = format!(
                        "ALTER TABLE {qt} ALTER COLUMN {qc} SET GENERATED {next_kind_only};"
                    );
                    let down = format!(
                        "ALTER TABLE {qt} ALTER COLUMN {qc} SET GENERATED {prev_kind_only};"
                    );
                    (up, down, "kind IDENTITY", None)
                }
                (None, None) => {
                    unreachable!("ColumnChange::SetIdentity is only emitted when from != to")
                }
            }
        }
        // Phase 8.5 djogi#217 — `COMMENT ON COLUMN` lowering.
        //
        // `COMMENT ON COLUMN` is its own top-level statement, not an
        // `ALTER TABLE … ALTER COLUMN` shape; routing it through
        // `emit_alter_column` is convenience (the differ exposes it
        // as a column-level change), not a SQL-shape claim. The
        // `label_suffix` reflects this distinction so the migration
        // log line reads `AlterColumn {table}.{column} (set
        // COMMENT)` even though no `ALTER TABLE` is emitted.
        //
        // The composer renders a setting-independent SQL string
        // literal; the differ filters identical pairs upstream so
        // `(None, None)` and `(Some(a), Some(a))` never reach this arm.
        ColumnChange::SetComment { from, to } => {
            let up = render_comment_on_column(table, column, to.as_deref());
            let down = render_comment_on_column(table, column, from.as_deref());
            (up, down, "set COMMENT", None)
        }
    };
    OperationSql {
        label: format!("AlterColumn {table}.{column} ({label_suffix})"),
        up,
        down,
        lossy,
    }
}

fn emit_add_foreign_key(table: &str, column: &str, fk: &ForeignKeySchema) -> OperationSql {
    let up = render_add_fk(table, column, fk);
    let constraint = fk_constraint_name(table, column);
    let qcons = quote_ident(&constraint);
    let qt = quote_ident(table);
    let down = format!("ALTER TABLE {qt} DROP CONSTRAINT {qcons};");
    OperationSql {
        label: format!("AddForeignKey {table}.{column}"),
        up,
        down,
        lossy: None,
    }
}

fn emit_drop_foreign_key(table: &str, column: &str, fk: &ForeignKeySchema) -> OperationSql {
    let constraint = fk_constraint_name(table, column);
    let qcons = quote_ident(&constraint);
    let qt = quote_ident(table);
    let up = format!("ALTER TABLE {qt} DROP CONSTRAINT {qcons};");
    // Down side reconstructs the FK from the carried schema — the
    // cascade discipline + target round-trip cleanly so rollback is
    // structurally lossless. Codex T3 review B-3 fixed an earlier
    // bug where the rollback was a SQL comment because `DropForeignKey`
    // carried only `(table, column)`.
    let down = render_add_fk(table, column, fk);
    OperationSql {
        label: format!("DropForeignKey {table}.{column}"),
        up,
        down,
        // No `lossy` warning — the FK definition is fully recoverable
        // from the carried schema. A future change that loses
        // information from the diff payload would have to re-introduce
        // this marker.
        lossy: None,
    }
}

/// Render a single `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY
/// ... REFERENCES ... ON DELETE ...;` statement. Shared between
/// [`emit_add_foreign_key`] and [`emit_drop_foreign_key`]'s rollback
/// path so the two cannot drift.
fn render_add_fk(table: &str, column: &str, fk: &ForeignKeySchema) -> String {
    let constraint = fk_constraint_name(table, column);
    let qcons = quote_ident(&constraint);
    let qt = quote_ident(table);
    let qc = quote_ident(column);
    let qref_t = quote_ident(&fk.ref_table);
    let qref_c = quote_ident(&fk.ref_column);
    let cascade = on_delete_sql(fk.on_delete);
    let deferrable = render_deferrable_clause(fk.deferrable, fk.initially_deferred);
    format!(
        "ALTER TABLE {qt} ADD CONSTRAINT {qcons} \
         FOREIGN KEY ({qc}) REFERENCES {qref_t} ({qref_c}) \
         ON DELETE {cascade}{deferrable};"
    )
}

fn render_deferrable_clause(deferrable: bool, initially_deferred: bool) -> &'static str {
    if !deferrable {
        ""
    } else if initially_deferred {
        " DEFERRABLE INITIALLY DEFERRED"
    } else {
        " DEFERRABLE INITIALLY IMMEDIATE"
    }
}

fn emit_add_index(idx: &IndexSchema) -> OperationSql {
    let mut up = String::with_capacity(128);
    let create = match idx.kind {
        IndexKindSchema::NonUnique => "CREATE INDEX",
        IndexKindSchema::UniqueConstraint | IndexKindSchema::UniqueIndex => "CREATE UNIQUE INDEX",
    };
    let _ = write!(up, "{create}");
    if idx.requires_out_of_transaction {
        up.push_str(" CONCURRENTLY");
    }
    let qname = quote_ident(&idx.name);
    let qtable = quote_ident(&idx.table);
    let _ = write!(
        up,
        " {qname} ON {qtable} USING {}",
        index_method(idx.index_type)
    );
    up.push(' ');
    write_index_target(&mut up, &idx.target);
    if !idx.include.is_empty() {
        up.push_str(" INCLUDE (");
        let mut first = true;
        for col in &idx.include {
            if !first {
                up.push_str(", ");
            }
            first = false;
            up.push_str(&quote_ident(col));
        }
        up.push(')');
    }
    if idx.nulls_not_distinct {
        up.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(pred) = &idx.predicate {
        let _ = write!(up, " WHERE {pred}");
    }
    up.push(';');

    let mut down = String::with_capacity(64);
    down.push_str("DROP INDEX");
    if idx.requires_out_of_transaction {
        down.push_str(" CONCURRENTLY");
    }
    let _ = write!(down, " {qname};");

    OperationSql {
        label: format!("AddIndex {}", idx.name),
        up,
        down,
        lossy: None,
    }
}

fn emit_drop_index(idx: &IndexSchema) -> OperationSql {
    let qname = quote_ident(&idx.name);
    let mut up = String::with_capacity(64);
    up.push_str("DROP INDEX");
    if idx.requires_out_of_transaction {
        up.push_str(" CONCURRENTLY");
    }
    let _ = write!(up, " {qname};");

    // The down side recreates the index from the carried IndexSchema.
    // DropIndex now carries the full schema (per T2 fixup B-4), so we
    // can emit a real recreate without the prior "comment-only"
    // limitation. The recreate is structurally lossless; only the
    // index data has to be rebuilt by Postgres.
    let down = recreate_index_sql(idx);

    OperationSql {
        label: format!("DropIndex {}", idx.name),
        up,
        down,
        // Rollback is structurally clean (full IndexSchema is in the
        // diff). Mark the warning anyway so operators see a "this
        // rebuild may be expensive" note when reviewing the file.
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::DropIndex,
            detail: format!(
                "index `{}` dropped — rollback rebuilds the index, which may \
                 take significant time on large tables",
                idx.name
            ),
        }),
    }
}

/// Render the body of an `EXCLUDE` constraint clause — the part after
/// `EXCLUDE` up to (but not including) the trailing semicolon.
///
/// Used by both [`emit_add_exclusion_constraint`] and the inline form
/// inside `emit_add_table` (PR 7 task 4). Produces `USING <method>
/// (<expr1> WITH <op1>, <expr2> WITH <op2>) [WHERE (<predicate>)]
/// [DEFERRABLE [INITIALLY DEFERRED]]`. No leading whitespace and no
/// trailing semicolon.
fn render_exclusion_body(exclusion: &ExclusionConstraintSchema) -> String {
    let mut body = String::with_capacity(64);
    let _ = write!(body, "EXCLUDE USING {} (", exclusion.using);
    for (idx, elem) in exclusion.elements.iter().enumerate() {
        if idx > 0 {
            body.push_str(", ");
        }
        let _ = write!(body, "{} WITH {}", elem.expr, elem.with_operator);
    }
    body.push(')');
    if let Some(where_clause) = &exclusion.where_clause {
        let _ = write!(body, " WHERE ({where_clause})");
    }
    if exclusion.deferrable {
        body.push_str(" DEFERRABLE");
        if exclusion.initially_deferred {
            body.push_str(" INITIALLY DEFERRED");
        }
    }
    body
}

fn emit_add_exclusion_constraint(
    table: &str,
    exclusion: &ExclusionConstraintSchema,
) -> OperationSql {
    let qt = quote_ident(table);
    let qname = quote_ident(&exclusion.name);
    let body = render_exclusion_body(exclusion);
    let up = format!("ALTER TABLE {qt} ADD CONSTRAINT {qname} {body};");
    let down = format!("ALTER TABLE {qt} DROP CONSTRAINT {qname};");
    OperationSql {
        label: format!("AddExclusionConstraint {table}.{}", exclusion.name),
        up,
        down,
        lossy: None,
    }
}

fn emit_drop_exclusion_constraint(
    table: &str,
    name: &str,
    exclusion: &ExclusionConstraintSchema,
) -> OperationSql {
    let qt = quote_ident(table);
    let qname = quote_ident(name);
    let up = format!("ALTER TABLE {qt} DROP CONSTRAINT {qname};");
    let body = render_exclusion_body(exclusion);
    let down = format!("ALTER TABLE {qt} ADD CONSTRAINT {qname} {body};");
    OperationSql {
        label: format!("DropExclusionConstraint {table}.{name}"),
        up,
        down,
        lossy: None,
    }
}

fn emit_add_enum(e: &EnumSchema) -> OperationSql {
    let qname = quote_ident(&e.name);
    let mut up = String::with_capacity(64);
    let _ = write!(up, "CREATE TYPE {qname} AS ENUM (");
    let mut first = true;
    for v in &e.variants {
        if !first {
            up.push_str(", ");
        }
        first = false;
        up.push_str(&quote_string_literal(v));
    }
    up.push_str(");");
    let down = format!("DROP TYPE {qname};");
    OperationSql {
        label: format!("AddEnum {}", e.name),
        up,
        down,
        lossy: None,
    }
}

fn emit_drop_enum(name: &str) -> OperationSql {
    let qname = quote_ident(name);
    let up = format!("DROP TYPE {qname};");
    let down = format!(
        "-- LOSSY ROLLBACK: cannot reconstruct enum `{name}` from the diff.\n\
         -- The original variant list is not present in DropEnum's payload.\n\
         -- Rollback must be hand-written if needed."
    );
    OperationSql {
        label: format!("DropEnum {name}"),
        up,
        down,
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::DropEnum,
            detail: format!("enum `{name}` dropped — variant list not recoverable from the diff"),
        }),
    }
}

fn emit_add_enum_variant(
    enum_name: &str,
    variant: &str,
    anchor: Option<&EnumVariantAnchor>,
) -> OperationSql {
    let qname = quote_ident(enum_name);
    let lit = quote_string_literal(variant);
    // The differ resolves the anchor variant against the new
    // variant list, picking BEFORE when a post-anchor exists and
    // AFTER when only a pre-anchor exists, else None for tail
    // appends. Codex T3 review B-2 fixed an earlier bug where the
    // emitter unconditionally appended (no positional clause)
    // regardless of where the differ placed the variant.
    let up = match anchor {
        None => format!("ALTER TYPE {qname} ADD VALUE {lit};"),
        Some(EnumVariantAnchor {
            variant: anchor_variant,
            kind,
        }) => {
            let anchor_lit = quote_string_literal(anchor_variant);
            let direction = match kind {
                EnumVariantAnchorKind::Before => "BEFORE",
                EnumVariantAnchorKind::After => "AFTER",
            };
            format!("ALTER TYPE {qname} ADD VALUE {lit} {direction} {anchor_lit};")
        }
    };
    // Postgres has no `DROP VALUE`. Rollback is lossy in the same
    // structural sense as DropEnum — mark accordingly.
    let down = format!(
        "-- LOSSY ROLLBACK: Postgres has no `ALTER TYPE ... DROP VALUE`.\n\
         -- Rolling back the addition of `{variant}` to enum `{enum_name}`\n\
         -- requires rebuilding the type. Hand-write the rollback if needed."
    );
    OperationSql {
        label: format!("AddEnumVariant {enum_name}:{variant}"),
        up,
        down,
        lossy: Some(LossyRollbackWarning {
            kind: LossyRollbackKind::DropEnum,
            detail: format!(
                "enum variant `{enum_name}:{variant}` added — Postgres has no \
                 native `DROP VALUE`; rollback requires a type rebuild"
            ),
        }),
    }
}

fn emit_rename_app(from: &str, to: &str) -> OperationSql {
    // RenameApp emits NO ddl. The migration engine handles the
    // folder move and the ledger UPDATE outside the standard SQL
    // path (per v3 plan §6 "Rename exception to append-only
    // ledger"). T3's job is to surface the operation as a
    // metadata-only segment so the runner dispatches it correctly.
    let up = format!(
        "-- METADATA-ONLY: rename app `{from}` to `{to}`.\n\
         -- Folder rename + djogi_schema_migrations.app_label UPDATE happen\n\
         -- outside the standard SQL emitter (handled by T6 compose / T4 runner)."
    );
    let down = format!(
        "-- METADATA-ONLY: reverse rename `{to}` -> `{from}`.\n\
         -- Folder rename + djogi_schema_migrations.app_label UPDATE happen\n\
         -- outside the standard SQL emitter."
    );
    OperationSql {
        label: format!("RenameApp {from} -> {to}"),
        up,
        down,
        lossy: None,
    }
}

fn emit_move_model_between_apps(model: &str, from_app: &str, to_app: &str) -> OperationSql {
    let up = format!(
        "-- METADATA-ONLY: move model `{model}` from app `{from_app}` to app `{to_app}`.\n\
         -- Folder move + djogi_schema_migrations.app_label UPDATE happen outside\n\
         -- the standard SQL emitter (handled by T6 compose / T4 runner)."
    );
    let down = format!(
        "-- METADATA-ONLY: reverse move `{model}` from `{to_app}` back to `{from_app}`.\n\
         -- Folder move + djogi_schema_migrations.app_label UPDATE happen outside\n\
         -- the standard SQL emitter."
    );
    OperationSql {
        label: format!("MoveModelBetweenApps {model} ({from_app} -> {to_app})"),
        up,
        down,
        lossy: None,
    }
}

/// Emit `COMMENT ON TABLE <qt> IS E'<escaped-to>'` (or `IS NULL` when
/// the comment is cleared), plus the symmetric down side restoring
/// `from`. Phase 8.5 Cluster 4 (djogi#217).
///
/// The differ filters identical pairs upstream, so `(None, None)` and
/// `(Some(a), Some(b))` with `a == b` never reach this fn in practice.
/// The defensive arm emits no-op SQL comments so a bug in the differ
/// surfaces as visible no-op output rather than malformed SQL.
fn emit_set_table_comment(table: &str, from: Option<&str>, to: Option<&str>) -> OperationSql {
    let qt = quote_ident(table);
    let render = |value: Option<&str>| -> String {
        match value {
            Some(text) => format!("COMMENT ON TABLE {qt} IS {};", render_comment_literal(text)),
            None => format!("COMMENT ON TABLE {qt} IS NULL;"),
        }
    };
    let up = render(to);
    let down = render(from);
    OperationSql {
        label: format!("SetTableComment {table}"),
        up,
        down,
        // `COMMENT ON` is a catalog-only write with no row touch —
        // both up and down are losslessly recoverable from the carried
        // `from` / `to`. No lossy marker.
        lossy: None,
    }
}

/// Emit reversible `ALTER TABLE ... SET/RESET (...)` storage-parameter
/// metadata changes. Phase 8.5 Cluster 4 (djogi#218).
fn emit_set_storage_params(
    table: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<OperationSql, SqlEmitError> {
    let render = |reset: Option<&str>, set: Option<&str>| -> Result<String, SqlEmitError> {
        let mut out = String::new();
        if let Some(params) = reset {
            out.push_str(&render_reset_storage_params(table, params)?);
        }
        if let Some(params) = set {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&render_set_storage_params(table, params)?);
        }
        Ok(if out.is_empty() {
            format!(
                "-- no-op storage parameter change for {}",
                quote_ident(table)
            )
        } else {
            out
        })
    };
    Ok(OperationSql {
        label: format!("SetStorageParams {table}"),
        up: render(from, to)?,
        down: render(to, from)?,
        lossy: None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StorageParamEntry {
    key: String,
    value: String,
}

fn render_set_storage_params(table: &str, params: &str) -> Result<String, SqlEmitError> {
    let qt = quote_ident(table);
    let entries = parse_storage_params_for_sql(params)?;
    Ok(format!(
        "ALTER TABLE {qt} SET ({});",
        render_storage_param_entries(&entries)
    ))
}

fn render_reset_storage_params(table: &str, params: &str) -> Result<String, SqlEmitError> {
    let qt = quote_ident(table);
    let entries = parse_storage_params_for_sql(params)?;
    let keys = entries
        .iter()
        .map(|entry| entry.key.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("ALTER TABLE {qt} RESET ({keys});"))
}

fn parse_storage_params_for_sql(params: &str) -> Result<Vec<StorageParamEntry>, SqlEmitError> {
    parse_storage_params(params).map_err(|reason| SqlEmitError::InvalidStorageParams {
        params: params.to_string(),
        reason,
    })
}

fn parse_storage_params(params: &str) -> Result<Vec<StorageParamEntry>, String> {
    if params.trim().is_empty() {
        return Err(
            "storage_params must be a non-empty comma-separated key=value list".to_string(),
        );
    }

    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    for part in params.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("storage_params entries must not be empty".to_string());
        }
        let Some((key, value)) = part.split_once('=') else {
            return Err(
                "storage_params entries must use key=value form separated by commas".to_string(),
            );
        };
        if value.contains('=') {
            return Err("storage_params entries must contain exactly one `=`".to_string());
        }

        let key = key.trim();
        let value = value.trim();
        validate_storage_param_key(key)?;
        validate_storage_param_value(value)?;

        let key = key.to_ascii_lowercase();
        if !seen.insert(key.clone()) {
            return Err(format!("duplicate storage_params key `{key}`"));
        }
        entries.push(StorageParamEntry {
            key,
            value: value.to_string(),
        });
    }

    Ok(entries)
}

fn validate_storage_param_key(key: &str) -> Result<(), String> {
    let bytes = key.as_bytes();
    if bytes.is_empty() {
        return Err("storage_params keys must not be empty".to_string());
    }
    if bytes.len() > 63 {
        return Err("storage_params keys must be at most 63 bytes".to_string());
    }
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return Err(
            "storage_params keys must start with an ASCII letter or underscore".to_string(),
        );
    }
    if !bytes
        .iter()
        .skip(1)
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(
            "storage_params keys must be plain ASCII reloption names; dotted keys are not supported"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_storage_param_value(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err("storage_params values must not be empty".to_string());
    }
    if is_storage_param_word(bytes) {
        if is_storage_param_sql_control_word(bytes) {
            return Err(
                "storage_params values must not be SQL statement/control words".to_string(),
            );
        }
        return Ok(());
    }
    if is_storage_param_number(bytes) {
        return Ok(());
    }
    Err(
        "storage_params values must be bare words or decimal numbers; quotes, comments, commas, \
         parentheses, semicolons, and SQL expressions are not supported"
            .to_string(),
    )
}

fn is_storage_param_word(bytes: &[u8]) -> bool {
    (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn is_storage_param_sql_control_word(bytes: &[u8]) -> bool {
    let word = bytes
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    matches!(
        word.as_slice(),
        b"alter"
            | b"begin"
            | b"call"
            | b"comment"
            | b"commit"
            | b"copy"
            | b"create"
            | b"delete"
            | b"do"
            | b"drop"
            | b"execute"
            | b"from"
            | b"grant"
            | b"insert"
            | b"reset"
            | b"revoke"
            | b"rollback"
            | b"select"
            | b"set"
            | b"table"
            | b"truncate"
            | b"union"
            | b"update"
            | b"where"
    )
}

fn is_storage_param_number(bytes: &[u8]) -> bool {
    let mut seen_dot = false;
    let mut digits_before_dot = 0usize;
    let mut digits_after_dot = 0usize;

    for byte in bytes {
        if byte.is_ascii_digit() {
            if seen_dot {
                digits_after_dot += 1;
            } else {
                digits_before_dot += 1;
            }
        } else if *byte == b'.' && !seen_dot {
            seen_dot = true;
        } else {
            return false;
        }
    }

    digits_before_dot > 0 && (!seen_dot || digits_after_dot > 0)
}

fn render_storage_param_entries(entries: &[StorageParamEntry]) -> String {
    let mut out = String::new();
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&entry.key);
        out.push('=');
        out.push_str(&entry.value);
    }
    out
}

/// Emit reversible `ALTER TABLE ... SET TABLESPACE ...` metadata
/// changes. `None` lowers to `pg_default`, Djogi's representation for
/// "no explicit tablespace". Phase 8.5 Cluster 4 (djogi#219).
fn emit_set_tablespace(table: &str, from: Option<&str>, to: Option<&str>) -> OperationSql {
    OperationSql {
        label: format!("SetTablespace {table}"),
        up: render_set_tablespace(table, to),
        down: render_set_tablespace(table, from),
        lossy: None,
    }
}

fn render_set_tablespace(table: &str, tablespace: Option<&str>) -> String {
    let qt = quote_ident(table);
    let qs = quote_ident(tablespace.unwrap_or("pg_default"));
    format!("ALTER TABLE {qt} SET TABLESPACE {qs};")
}

/// Emit `COMMENT ON COLUMN <qt>.<qc> IS E'<escaped>'` (or `IS NULL`).
/// Shared between [`emit_alter_column`]'s `SetComment` arm and the
/// inline emission that follows `CREATE TABLE` / `ADD COLUMN` for
/// fields that ship with `#[field(comment = "…")]` set on initial
/// creation. Phase 8.5 Cluster 4 (djogi#217).
fn render_comment_on_column(table: &str, column: &str, value: Option<&str>) -> String {
    let qt = quote_ident(table);
    let qc = quote_ident(column);
    match value {
        Some(text) => format!(
            "COMMENT ON COLUMN {qt}.{qc} IS {};",
            render_comment_literal(text)
        ),
        None => format!("COMMENT ON COLUMN {qt}.{qc} IS NULL;"),
    }
}

/// Render a Postgres escape string literal (`E'…'`) for comment text.
///
/// `E'…'` has explicit backslash-escape semantics regardless of the
/// session's `standard_conforming_strings` setting, so we double both
/// apostrophes and backslashes before inlining adopter-provided comment
/// text.
///
/// **Scope.** Used by the `COMMENT ON` emission path (djogi#217) only.
/// Every other adopter SQL path treats the value as raw SQL and does
/// NOT escape — adopters writing `#[field(check = "…")]` are
/// responsible for the SQL fragment's correctness themselves. The
/// helper is scope-named so future callers cannot accidentally adopt it
/// for raw-SQL paths.
fn render_comment_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    out.push_str("E'");
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Emit `"ident"` — double-quote and escape any embedded `"` per
/// the SQL standard (each embedded quote becomes `""`). The
/// descriptor / projection layer rejects embedded quotes already, so
/// the loop is structural belt-and-braces.
pub(crate) fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for b in name.as_bytes() {
        if *b == b'"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(*b as char);
        }
    }
    out.push('"');
    out
}

/// Emit a SQL string literal with single-quote doubling — `it's` ->
/// `'it''s'`. Used for enum variant labels and other literal
/// payloads. Identifier quoting goes through [`quote_ident`].
pub(crate) fn quote_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for b in value.as_bytes() {
        if *b == b'\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(*b as char);
        }
    }
    out.push('\'');
    out
}

/// Constraint name for a column-level CHECK constraint, e.g.
/// `users_email_check`. Deterministic from `(table, column)`.
fn check_constraint_name(table: &str, column: &str) -> String {
    truncate_constraint(format!("{table}_{column}_check"))
}

/// Constraint name for a column-level UNIQUE constraint, e.g.
/// `users_email_key`. Matches the descriptor's `index_name` "key"
/// suffix so the standard naming layer stays consistent.
fn unique_constraint_name(table: &str, column: &str) -> String {
    truncate_constraint(format!("{table}_{column}_key"))
}

/// Constraint name for a column-level FK constraint, e.g.
/// `posts_author_id_fkey`. Postgres's auto-generated FK names use
/// the `_fkey` suffix; we follow suit.
///
/// `pub(crate)` so the runtime
/// `DjogiContext::defer_constraints` validator (Phase 8.5 #169) can
/// reach the same composition the migration emitter uses, keeping
/// declarative-time and runtime constraint names in lockstep.
pub(crate) fn fk_constraint_name(table: &str, column: &str) -> String {
    truncate_constraint(format!("{table}_{column}_fkey"))
}

/// Truncate a constraint identifier to the Postgres 63-byte limit.
///
/// Layout for long names: 54-byte stem + `_` + 8-char hex digest =
/// exactly 63 bytes. Constraint names that already fit are returned
/// verbatim.
///
/// **Why 54 + 1 + 8 = 63 (and not 55 + 1 + 8 = 64).** Postgres's
/// usable identifier limit is 63 bytes
/// (`NAMEDATALEN - 1` on a default build); names longer than that
/// are silently truncated by the server, which would let two
/// distinct constraint names collide post-truncation. We size the
/// stem so the total stays at exactly the limit.
///
/// The hash uses `std::hash::DefaultHasher` (SipHash-1-3) — same
/// primitive as [`crate::descriptor::index_name`]. Determinism
/// within a single process is sufficient because the constraint
/// name is computed once per emission and is part of the migration
/// file the operator commits.
fn truncate_constraint(name: String) -> String {
    if name.len() <= 63 {
        return name;
    }
    use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
    let mut h =
        BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().build_hasher();
    h.write(name.as_bytes());
    let raw = h.finish();
    let digest = format!("{:08x}", raw as u32);
    let stem: String = name.as_bytes()[..54].iter().map(|b| *b as char).collect();
    format!("{stem}_{digest}")
}

/// Render a column's full `<name> <type> [NOT NULL] [DEFAULT ...]
/// [UNIQUE] [REFERENCES ...] [PRIMARY KEY]` definition into `out`.
///
/// `PRIMARY KEY` is inlined here only for the single-column,
/// non-Composite, non-Custom PK shapes — those go through
/// [`pk_table_clause`] at the table level.
///
/// **`table` parameter** is needed to compute the deterministic
/// constraint name for an inline CHECK clause
/// (`{table}_{column}_check`, see [`check_constraint_name`]).
/// Postgres auto-generates constraint names for unnamed CHECKs in
/// inconsistent shapes (`{table}_check`, `{table}_check1`, etc.); the
/// explicit `CONSTRAINT` keyword makes the name deterministic so:
///
///   1. The ALTER TABLE DROP CONSTRAINT path from the differ
///      ([`ColumnChange::SetCheck`] with `to: None`) reaches the same
///      constraint slot inline CREATE TABLE produced.
///   2. Adopter-facing error messages reference a predictable
///      constraint name that mirrors the migration emitter's
///      ALTER-TABLE path.
fn write_column_definition(out: &mut String, col: &ColumnSchema, table: &str) {
    let qn = quote_ident(&col.name);
    out.push_str(&qn);
    out.push(' ');
    out.push_str(&col.sql_type);
    if !col.nullable {
        out.push_str(" NOT NULL");
    }
    // Identity columns (Cluster E #86 fix) — `GENERATED BY DEFAULT AS
    // IDENTITY` / `GENERATED ALWAYS AS IDENTITY` is part of the column
    // definition, separate from both DEFAULT (a value expression) and
    // computed-generated (an expression-derived value). Identity cannot
    // coexist with DEFAULT on the same column (Postgres rejects), and
    // cannot coexist with computed-generated either (mutually exclusive
    // semantics — identity uses a sequence, computed uses an
    // expression). The projection guarantees the mutual exclusion;
    // this branch fires only when `identity.is_some()`.
    if let Some(identity) = col.identity {
        out.push(' ');
        out.push_str(identity.sql_clause());
    } else if let Some(generated) = &col.generated {
        // Stored generated columns replace DEFAULT — Postgres rejects
        // both clauses on the same column. The GENERATED clause
        // carries the value source instead.
        let stored_clause = if generated.stored {
            "STORED"
        } else {
            "VIRTUAL"
        };
        let _ = write!(
            out,
            " GENERATED ALWAYS AS ({}) {stored_clause}",
            generated.expression,
        );
    } else if let Some(def) = &col.default_sql {
        let _ = write!(out, " DEFAULT {def}");
    }
    if col.unique {
        out.push_str(" UNIQUE");
    }
    if let Some(check) = &col.check {
        // Emit `CONSTRAINT <name> CHECK (...)` rather than a bare
        // `CHECK (...)` so the constraint name matches the
        // ALTER-TABLE-emitted shape from
        // [`ColumnChange::SetCheck`] (with `to: Some(_)`). The differ's
        // DROP / AMEND paths reference the constraint by name; an
        // inline auto-named CHECK would create a different name
        // (`{table}_check` / `{table}_check1` per Postgres's
        // auto-naming) and the differ would fail to drop it.
        let constraint = check_constraint_name(table, &col.name);
        let qcons = quote_ident(&constraint);
        let _ = write!(out, " CONSTRAINT {qcons} CHECK ({check})");
    }
    if let Some(fk) = &col.foreign_key {
        // Cascade source-of-truth lives on `ForeignKeySchema.on_delete`
        // — the standalone `AddForeignKey` / `DropForeignKey` paths
        // already read it from there. Codex T3 round-2 review A-1
        // flagged that the inline-FK path was reading `ColumnSchema
        // .on_delete` instead, splitting the SQL emitter across two
        // sources for the same value. Both fields are populated from
        // the same descriptor input today, but two read sites invite
        // future drift; consolidating on `fk.on_delete` removes that
        // hazard. The mirrored `ColumnSchema.on_delete` field stays
        // for adopters that walk columns directly — only the SQL
        // emitter no longer reads it.
        //
        // Emit `CONSTRAINT <name> REFERENCES (...)` rather than a bare
        // `REFERENCES (...)` so the FK name matches the deterministic
        // shape from [`fk_constraint_name`]. Postgres auto-names
        // unnamed FKs as `{table}_{column}_fkey`, which agrees with
        // the Djogi convention for short names but silently TAIL-
        // TRUNCATES at 63 bytes for long names — whereas Djogi's
        // convention preserves a 54-byte stem and appends an 8-char
        // hash. Two non-matching names mean the runtime
        // [`DjogiContext::defer_constraints`] validator (Phase 8.5
        // #169) would happily approve a `SET CONSTRAINTS
        // "<djogi_hashed_name>" DEFERRED` that Postgres rejects with
        // `42704` (constraint does not exist). Explicit naming locks
        // the emitter and the validator into lockstep for any name
        // length. (GPT-5.5 xhigh BLOCK follow-up for djogi#169.)
        let constraint = fk_constraint_name(table, &col.name);
        let qcons = quote_ident(&constraint);
        let qref_t = quote_ident(&fk.ref_table);
        let qref_c = quote_ident(&fk.ref_column);
        let _ = write!(out, " CONSTRAINT {qcons} REFERENCES {qref_t} ({qref_c})");
        let _ = write!(out, " ON DELETE {}", on_delete_sql(fk.on_delete));
        out.push_str(render_deferrable_clause(
            fk.deferrable,
            fk.initially_deferred,
        ));
    }
}

/// Decide whether the table-level PK clause is required. Returns
/// `Some(clause)` for Composite / Custom / multi-column shapes;
/// returns `None` when the PK is the canonical single-column `id`
/// shape so the emitter inlines `PRIMARY KEY` on the column itself
/// (handled inside [`emit_add_table`] via the implicit pattern in
/// `default_sql` for HeerId/RanjId/Serial defaults).
///
/// **Inline policy**. We always emit the PK at the table level for
/// composite shapes; for single-column shapes we still emit at the
/// table level for clarity ("the PK shape lives in one place,
/// regardless of whether it is one column or many"). This mirrors
/// how `pg_dump` formats `CREATE TABLE` output and keeps the
/// composite / non-composite output paths identical.
fn pk_table_clause(t: &TableSchema) -> Option<String> {
    if matches!(t.primary_key.kind, PkKindSchema::None) || t.primary_key.columns.is_empty() {
        return None;
    }
    let mut s = String::with_capacity(64);
    s.push_str("PRIMARY KEY (");
    let mut first = true;
    for col in &t.primary_key.columns {
        if !first {
            s.push_str(", ");
        }
        first = false;
        s.push_str(&quote_ident(col));
    }
    s.push(')');
    Some(s)
}

fn partition_clause(p: &PartitionSchema) -> String {
    match p {
        PartitionSchema::Range { column } => {
            format!("PARTITION BY RANGE ({})", quote_ident(column))
        }
        PartitionSchema::Hash { column, partitions } => {
            // Postgres's declarative partitioning for HASH does not
            // accept an inline `PARTITIONS n` count — child tables
            // must be created separately with `FOR VALUES WITH
            // (modulus n, remainder k)`. We emit only the parent
            // declaration; the child-table machinery is the
            // partitioning task's territory. Capture the partition
            // count in a comment so the partition manager (later
            // task) can pick it up from the migration text.
            format!(
                "PARTITION BY HASH ({}) /* partitions = {} */",
                quote_ident(column),
                partitions
            )
        }
    }
}

fn write_index_target(out: &mut String, target: &IndexTargetSchema) {
    match target {
        IndexTargetSchema::Columns(cols) => {
            out.push('(');
            let mut first = true;
            for c in cols {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                write_index_column(out, c);
            }
            out.push(')');
        }
        IndexTargetSchema::Expression(expr) => {
            // Expression-form indexes always need the doubled parens
            // — `CREATE INDEX ... ON t ((expr))` per Postgres docs.
            let _ = write!(out, "(({expr}))");
        }
    }
}

fn write_index_column(out: &mut String, c: &IndexColumnSchema) {
    out.push_str(&quote_ident(&c.name));
    if let Some(opclass) = &c.opclass {
        let _ = write!(out, " {opclass}");
    }
    match c.order {
        IndexOrderSchema::Asc => {} // Postgres default — omit for clean output.
        IndexOrderSchema::Desc => out.push_str(" DESC"),
    }
    match c.nulls {
        IndexNullsOrderSchema::Default => {} // emitter omits the clause.
        IndexNullsOrderSchema::First => out.push_str(" NULLS FIRST"),
        IndexNullsOrderSchema::Last => out.push_str(" NULLS LAST"),
    }
}

/// Recreate a previously-dropped index from its full schema. Used
/// for `DropIndex`'s rollback side and re-usable when a future
/// task needs to clone an index.
///
/// Implementation reuses [`emit_add_index`] so the recreate SQL is
/// formatted identically to the original AddIndex emission — there
/// is no parallel formatter that could drift.
fn recreate_index_sql(idx: &IndexSchema) -> String {
    emit_add_index(idx).up
}

fn index_method(t: IndexTypeSchema) -> &'static str {
    match t {
        IndexTypeSchema::BTree => "btree",
        IndexTypeSchema::Gin => "gin",
        IndexTypeSchema::Gist => "gist",
        IndexTypeSchema::Hash => "hash",
        IndexTypeSchema::Spgist => "spgist",
        IndexTypeSchema::Brin => "brin",
    }
}

fn on_delete_sql(d: OnDeleteSchema) -> &'static str {
    match d {
        OnDeleteSchema::Restrict => "RESTRICT",
        OnDeleteSchema::Cascade => "CASCADE",
        OnDeleteSchema::SetNull => "SET NULL",
        OnDeleteSchema::SetDefault => "SET DEFAULT",
        OnDeleteSchema::NoAction => "NO ACTION",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::schema::{
        AppliedSchema, ColumnSchema, ForeignKeySchema, IndexColumnSchema, IndexKindSchema,
        IndexNullsOrderSchema, IndexOrderSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
        OnDeleteSchema, PkKindSchema, PrimaryKeySchema, RelationKindSchema, TableSchema,
    };

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
        }
    }

    fn pk_id_heerid() -> PrimaryKeySchema {
        PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        }
    }

    fn id_column_heerid() -> ColumnSchema {
        ColumnSchema {
            default_sql: Some("heerid_next()".to_string()),
            ..col("id", "BIGINT", false)
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
            primary_key: pk_id_heerid(),
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

    fn table_with_amount_and_check(
        table: &str,
        sql_type: &str,
        check: Option<&str>,
    ) -> TableSchema {
        let mut t = synth_table(table);
        t.columns = vec![
            id_column_heerid(),
            ColumnSchema {
                check: check.map(|s| s.to_string()),
                comment: None,
                default_sql: None,
                foreign_key: None,
                generated: None,
                identity: None,
                index_type: None,
                indexed: false,
                max_length: None,
                name: "amount".to_string(),
                nullable: false,
                on_delete: None,
                outbox_exclude: false,
                rationale: None,
                relation_kind: None,
                renamed_from: None,
                sequence_within: None,
                sql_type: sql_type.to_string(),
                unique: false,
            },
        ];
        t
    }

    fn applied_schema_with_amount_check(sql_type: &str, check: Option<&str>) -> AppliedSchema {
        let mut models = std::collections::BTreeMap::new();
        models.insert(
            "widgets".to_string(),
            table_with_amount_and_check("widgets", sql_type, check),
        );
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: std::collections::BTreeMap::new(),
            format_version: "1".to_string(),
            generated_at: "2026-05-10T00:00:00Z".to_string(),
            indexes: vec![],
            models,
            registered_apps: vec!["".to_string()],
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

    // ── quote_ident ────────────────────────────────────────────────────

    #[test]
    fn quote_ident_quotes_simple_name() {
        assert_eq!(quote_ident("users"), "\"users\"");
    }

    #[test]
    fn quote_ident_doubles_embedded_quote() {
        // The descriptor layer rejects `"` in identifiers, but the
        // emitter still doubles the quote as belt-and-braces — a
        // future descriptor change cannot turn this into a SQL
        // injection vector.
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn quote_string_literal_doubles_embedded_quote() {
        assert_eq!(quote_string_literal("it's"), "'it''s'");
    }

    // ── AddTable ───────────────────────────────────────────────────────

    #[test]
    fn add_table_emits_create_table_with_columns_and_pk() {
        let t = synth_table("users");
        let sql = emit_add_table(&t);
        assert!(sql.up.starts_with("CREATE TABLE \"users\" ("));
        assert!(
            sql.up
                .contains("\"id\" BIGINT NOT NULL DEFAULT heerid_next()")
        );
        assert!(sql.up.contains("\"name\" TEXT"));
        assert!(sql.up.contains("PRIMARY KEY (\"id\")"));
        assert!(sql.up.ends_with(";"));
        assert_eq!(sql.down, "DROP TABLE \"users\";");
        assert!(sql.lossy.is_none());
    }

    #[test]
    fn add_table_with_partition_emits_partition_clause() {
        let mut t = synth_table("events");
        t.partition = Some(PartitionSchema::Range {
            column: "created_at".to_string(),
        });
        let sql = emit_add_table(&t);
        assert!(
            sql.up.contains("PARTITION BY RANGE (\"created_at\")"),
            "got: {}",
            sql.up
        );
    }

    #[test]
    fn add_table_with_hash_partition_carries_partition_count() {
        let mut t = synth_table("shards");
        t.partition = Some(PartitionSchema::Hash {
            column: "id".to_string(),
            partitions: 8,
        });
        let sql = emit_add_table(&t);
        assert!(sql.up.contains("PARTITION BY HASH (\"id\")"));
        assert!(sql.up.contains("/* partitions = 8 */"));
    }

    #[test]
    fn add_table_with_composite_pk_emits_composite_constraint() {
        let mut t = synth_table("memberships");
        t.primary_key = PrimaryKeySchema {
            columns: vec!["user_id".to_string(), "role_id".to_string()],
            kind: PkKindSchema::Composite,
        };
        t.columns = vec![
            col("user_id", "BIGINT", false),
            col("role_id", "BIGINT", false),
        ];
        let sql = emit_add_table(&t);
        assert!(
            sql.up.contains("PRIMARY KEY (\"user_id\", \"role_id\")"),
            "got: {}",
            sql.up
        );
    }

    #[test]
    fn add_table_with_no_pk_skips_constraint_clause() {
        let mut t = synth_table("audit_events");
        t.primary_key = PrimaryKeySchema {
            columns: Vec::new(),
            kind: PkKindSchema::None,
        };
        t.columns = vec![col("event", "TEXT", false)];
        let sql = emit_add_table(&t);
        assert!(!sql.up.contains("PRIMARY KEY"));
    }

    #[test]
    fn add_table_with_fk_column_inlines_references_clause() {
        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Cascade,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::Cascade),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col("user_id", "BIGINT", false)
        };
        let mut t = synth_table("posts");
        t.columns = vec![id_column_heerid(), fk_col];
        let sql = emit_add_table(&t);
        assert!(
            sql.up
                .contains("REFERENCES \"users\" (\"id\") ON DELETE CASCADE"),
            "got: {}",
            sql.up
        );
    }

    #[test]
    fn add_table_inline_fk_propagates_cascade_kind() {
        // Codex T3 round-2 review A-1: the inline-FK path inside
        // `CREATE TABLE` must read the cascade from
        // `ForeignKeySchema.on_delete` — the same source the
        // standalone `AddForeignKey` / `DropForeignKey` paths use.
        // Round-trip every variant. To exercise the precise contract,
        // we leave `ColumnSchema.on_delete` set to something different
        // from `ForeignKeySchema.on_delete` and assert that the FK's
        // value wins. (Today's projection populates both fields from
        // the same descriptor input; this test pins the SQL emitter
        // to the FK-side source so a future descriptor change that
        // splits the two cannot silently rewrite cascades.)
        for (cascade, expected) in [
            (OnDeleteSchema::Restrict, "ON DELETE RESTRICT"),
            (OnDeleteSchema::Cascade, "ON DELETE CASCADE"),
            (OnDeleteSchema::SetNull, "ON DELETE SET NULL"),
            (OnDeleteSchema::SetDefault, "ON DELETE SET DEFAULT"),
            (OnDeleteSchema::NoAction, "ON DELETE NO ACTION"),
        ] {
            let fk_col = ColumnSchema {
                foreign_key: Some(ForeignKeySchema {
                    deferrable: false,
                    initially_deferred: false,
                    on_delete: cascade,
                    ref_column: "id".to_string(),
                    ref_table: "users".to_string(),
                }),
                // Intentional mismatch: prove the emitter ignores the
                // column-level mirror. `Restrict` here would have been
                // the silently-wrong cascade under the prior code path.
                on_delete: Some(OnDeleteSchema::Restrict),
                relation_kind: Some(RelationKindSchema::ForeignKey),
                ..col("user_id", "BIGINT", false)
            };
            let mut t = synth_table("posts");
            t.columns = vec![id_column_heerid(), fk_col];
            let sql = emit_add_table(&t);
            assert!(
                sql.up.contains(expected),
                "inline FK cascade {cascade:?} must emit `{expected}`; \
                 got: {}",
                sql.up
            );
        }
    }

    #[test]
    fn render_create_table_with_deferrable_fk() {
        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: true,
                initially_deferred: true,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::Restrict),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col("user_id", "BIGINT", false)
        };
        let mut t = synth_table("posts");
        t.columns = vec![id_column_heerid(), fk_col];
        let sql = emit_add_table(&t);
        assert!(
            sql.up.contains(
                "REFERENCES \"users\" (\"id\") ON DELETE RESTRICT \
                 DEFERRABLE INITIALLY DEFERRED"
            ),
            "got: {}",
            sql.up
        );
    }

    #[test]
    fn render_create_table_with_immediately_deferrable_fk() {
        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: true,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::Restrict),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col("user_id", "BIGINT", false)
        };
        let mut t = synth_table("posts");
        t.columns = vec![id_column_heerid(), fk_col];
        let sql = emit_add_table(&t);
        assert!(
            sql.up.contains(
                "REFERENCES \"users\" (\"id\") ON DELETE RESTRICT \
                 DEFERRABLE INITIALLY IMMEDIATE"
            ),
            "got: {}",
            sql.up
        );
    }

    #[test]
    fn render_create_table_with_non_deferrable_fk() {
        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::Restrict),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col("user_id", "BIGINT", false)
        };
        let mut t = synth_table("posts");
        t.columns = vec![id_column_heerid(), fk_col];
        let sql = emit_add_table(&t);
        assert!(
            !sql.up.contains("DEFERRABLE"),
            "non-deferrable FK must not emit a deferrability clause: {}",
            sql.up
        );
    }

    #[test]
    fn add_table_inline_fk_names_constraint_explicitly_short_name() {
        // GPT-5.5 xhigh BLOCK follow-up for djogi#169: the inline-FK
        // path inside `CREATE TABLE` must emit `CONSTRAINT <name>
        // REFERENCES ...` so the runtime
        // `DjogiContext::defer_constraints` validator (which derives
        // the expected name via [`fk_constraint_name`]) and the
        // emitted DDL agree byte-for-byte. The short-name case is the
        // simpler half of the pair below — the conventional
        // `posts_user_id_fkey` fits inside Postgres' 63-byte identifier
        // limit, so Djogi's name and Postgres' auto-name happen to
        // agree. We still pin the explicit `CONSTRAINT ...` shape so a
        // future emitter regression that drops the keyword cannot
        // silently re-introduce the auto-naming reliance.
        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: true,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::Restrict),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col("user_id", "BIGINT", false)
        };
        let mut t = synth_table("posts");
        t.columns = vec![id_column_heerid(), fk_col];
        let sql = emit_add_table(&t);
        let expected_name = fk_constraint_name("posts", "user_id");
        assert_eq!(
            expected_name, "posts_user_id_fkey",
            "short-name sanity: the convention must produce the verbatim \
             `{{table}}_{{column}}_fkey` for inputs that fit inside 63 bytes",
        );
        assert!(
            sql.up
                .contains(" CONSTRAINT \"posts_user_id_fkey\" REFERENCES \"users\" (\"id\")"),
            "inline FK must emit explicit `CONSTRAINT <name> REFERENCES ...`; \
             got: {}",
            sql.up
        );
        // The existing surrounding clauses (cascade + deferrability)
        // must still emit unchanged.
        assert!(
            sql.up
                .contains("ON DELETE RESTRICT DEFERRABLE INITIALLY IMMEDIATE"),
            "explicit constraint name must preserve cascade + deferrability \
             order; got: {}",
            sql.up
        );
    }

    #[test]
    fn add_table_inline_fk_uses_djogi_hashed_name_for_long_identifiers() {
        // GPT-5.5 xhigh BLOCK regression for djogi#169: when the
        // conventional name `<table>_<column>_fkey` exceeds Postgres'
        // 63-byte identifier limit, Postgres' auto-naming TAIL-
        // TRUNCATES (lopping bytes off the right) while Djogi's
        // [`fk_constraint_name`] preserves a 54-byte stem and appends
        // an 8-char hex digest. The two names differ, so an unnamed
        // inline FK would name the constraint differently from what
        // the runtime `defer_constraints` validator expects, and a
        // `SET CONSTRAINTS "<djogi_hashed_name>" DEFERRED` would raise
        // SQLSTATE `42704` against Postgres' truncated name.
        //
        // Constraint-name math:
        //   table  = "djogi_some_very_long_table_for_fk_regression"  (44 bytes)
        //   column = "author_user_account_id_reference"              (31 bytes)
        //   "{table}_{column}_fkey" = 44 + 1 + 31 + 5 = 81 bytes  (>63)
        // Therefore the convention falls into the truncate branch and
        // emits `<54-byte stem>_<8 hex>` for a total of 63 bytes.
        let table = "djogi_some_very_long_table_for_fk_regression";
        let column = "author_user_account_id_reference";
        let conventional = format!("{table}_{column}_fkey");
        assert!(
            conventional.len() > 63,
            "test precondition: conventional name {conventional:?} \
             ({} bytes) must exceed Postgres' 63-byte limit so the \
             hashed branch is exercised",
            conventional.len(),
        );

        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: true,
                initially_deferred: true,
                on_delete: OnDeleteSchema::Cascade,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::Cascade),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col(column, "BIGINT", false)
        };
        let mut t = synth_table(table);
        t.columns = vec![id_column_heerid(), fk_col];
        let sql = emit_add_table(&t);

        let expected_name = fk_constraint_name(table, column);
        assert_eq!(
            expected_name.len(),
            63,
            "convention must produce exactly 63 bytes for over-long inputs; \
             got {} bytes: {expected_name}",
            expected_name.len(),
        );
        assert_ne!(
            expected_name, conventional,
            "the convention must NOT equal the conventional name when the \
             input would overflow; otherwise the regression is not exercised",
        );

        // The hashed name must appear verbatim inside the emitted DDL,
        // wrapped in the explicit `CONSTRAINT ... REFERENCES ...`
        // shape — proving the emitter and the runtime validator name
        // the same constraint slot.
        let expected_fragment =
            format!(" CONSTRAINT \"{expected_name}\" REFERENCES \"users\" (\"id\")");
        assert!(
            sql.up.contains(&expected_fragment),
            "emitted DDL must contain `{expected_fragment}` so the runtime \
             `defer_constraints` validator's expected name matches; got: {}",
            sql.up
        );

        // Cascade + deferrability behaviour must round-trip unchanged
        // — the fix touches only the constraint-name slot.
        assert!(
            sql.up
                .contains("ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"),
            "explicit constraint name must preserve cascade + deferrability \
             order; got: {}",
            sql.up
        );

        // The runtime validator's `fk_constraint_name(table, column)`
        // must reach the same string the emitter wrote. This pins the
        // lockstep contract directly: any future change that splits
        // the two sites would have to update both call sites for this
        // assertion to keep passing.
        let runtime_expected = fk_constraint_name(table, column);
        assert!(
            sql.up.contains(&format!("\"{runtime_expected}\"")),
            "runtime validator's derived name `{runtime_expected}` must \
             appear verbatim in emitted DDL; got: {}",
            sql.up
        );
    }

    #[test]
    fn add_column_inline_fk_uses_explicit_constraint_name() {
        // The ALTER-TABLE-ADD-COLUMN path also flows through
        // `write_column_definition`, so the explicit-CONSTRAINT fix
        // must apply there too — otherwise a new FK column added to
        // an existing table reintroduces the auto-naming hazard.
        let column = "author_user_account_id_reference";
        let fk_col = ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable: true,
                initially_deferred: false,
                on_delete: OnDeleteSchema::SetNull,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }),
            on_delete: Some(OnDeleteSchema::SetNull),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            nullable: true,
            ..col(column, "BIGINT", true)
        };
        let sql = emit_add_column("djogi_some_very_long_table_for_fk_regression", &fk_col);
        let expected_name =
            fk_constraint_name("djogi_some_very_long_table_for_fk_regression", column);
        assert_eq!(expected_name.len(), 63);
        assert!(
            sql.up.contains(&format!(
                " CONSTRAINT \"{expected_name}\" REFERENCES \"users\" (\"id\")"
            )),
            "ALTER TABLE ADD COLUMN inline FK must carry the explicit \
             `CONSTRAINT <hashed_name> REFERENCES ...` shape; got: {}",
            sql.up
        );
    }

    // ── DropTable + lossy ──────────────────────────────────────────────

    #[test]
    fn drop_table_marks_lossy_rollback() {
        let sql = emit_drop_table("orders");
        assert_eq!(sql.up, "DROP TABLE \"orders\";");
        assert!(sql.down.contains("LOSSY ROLLBACK"));
        let warn = sql.lossy.expect("lossy warning");
        assert!(matches!(warn.kind, LossyRollbackKind::DropTable));
    }

    // ── Column ops ─────────────────────────────────────────────────────

    #[test]
    fn add_column_emits_alter_table_add_column() {
        let c = col("email", "TEXT", false);
        let sql = emit_add_column("users", &c);
        assert_eq!(
            sql.up,
            "ALTER TABLE \"users\" ADD COLUMN \"email\" TEXT NOT NULL;"
        );
        assert_eq!(sql.down, "ALTER TABLE \"users\" DROP COLUMN \"email\";");
        assert!(sql.lossy.is_some(), "AddColumn rollback drops a column");
    }

    #[test]
    fn drop_column_marks_lossy_with_comment_only_down() {
        let sql = emit_drop_column("users", "legacy_id");
        assert_eq!(sql.up, "ALTER TABLE \"users\" DROP COLUMN \"legacy_id\";");
        assert!(sql.down.contains("LOSSY ROLLBACK"));
        let warn = sql.lossy.expect("lossy warning");
        assert!(matches!(warn.kind, LossyRollbackKind::DropColumn));
    }

    #[test]
    fn rename_column_round_trips_in_down() {
        let sql = emit_rename_column("users", "old_name", "new_name");
        assert_eq!(
            sql.up,
            "ALTER TABLE \"users\" RENAME COLUMN \"old_name\" TO \"new_name\";"
        );
        assert_eq!(
            sql.down,
            "ALTER TABLE \"users\" RENAME COLUMN \"new_name\" TO \"old_name\";"
        );
        assert!(sql.lossy.is_none());
    }

    #[test]
    fn alter_column_set_not_null_round_trips() {
        let sql = emit_alter_column("users", "email", &ColumnChange::SetNullable(false));
        assert_eq!(
            sql.up,
            "ALTER TABLE \"users\" ALTER COLUMN \"email\" SET NOT NULL;"
        );
        assert_eq!(
            sql.down,
            "ALTER TABLE \"users\" ALTER COLUMN \"email\" DROP NOT NULL;"
        );
    }

    #[test]
    fn alter_column_change_type_emits_using_cast() {
        let sql = emit_alter_column(
            "events",
            "amount",
            &ColumnChange::ChangeType {
                from: "TEXT".to_string(),
                to: "BIGINT".to_string(),
            },
        );
        assert!(sql.up.contains("TYPE BIGINT USING \"amount\"::BIGINT"));
        assert!(sql.down.contains("TYPE TEXT USING \"amount\"::TEXT"));
    }

    #[test]
    fn alter_column_set_check_uses_named_constraint() {
        let sql = emit_alter_column(
            "users",
            "email",
            &ColumnChange::SetCheck {
                from: None,
                to: Some("email <> ''".to_string()),
            },
        );
        assert!(sql.up.contains("ADD CONSTRAINT \"users_email_check\""));
        assert!(sql.up.contains("CHECK (email <> '')"));
        // Pure ADD has no prior to restore — rollback is DROP only.
        assert!(sql.down.contains("DROP CONSTRAINT \"users_email_check\""));
        assert!(
            !sql.down.contains("ADD CONSTRAINT"),
            "ADD-only rollback must not re-add: {}",
            sql.down
        );
    }

    // ── djogi#186 — type-derived integer-bound CHECKs ──────────────────────
    //
    // Mirrors `alter_column_set_check_uses_named_constraint` for every
    // Rust integer width that projects a CHECK expression. The
    // expression strings come from `migrate::projection::field_type_check`;
    // these tests pin the SQL the emitter wraps around them. Together
    // with the `field_type_check_*` tests in `projection.rs` this
    // covers the full descriptor → SQL pipeline.

    #[test]
    fn alter_column_set_check_for_i8_smallint() {
        let sql = emit_alter_column(
            "widgets",
            "maybe_signed_byte",
            &ColumnChange::SetCheck {
                from: None,
                to: Some(
                    "\"maybe_signed_byte\" >= -128 AND \"maybe_signed_byte\" <= 127".to_string(),
                ),
            },
        );
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"widgets_maybe_signed_byte_check\""),
            "i8 CHECK uses table+column constraint name: {}",
            sql.up
        );
        assert!(
            sql.up
                .contains("CHECK (\"maybe_signed_byte\" >= -128 AND \"maybe_signed_byte\" <= 127)"),
            "i8 CHECK expression wraps the projected bound: {}",
            sql.up
        );
        assert!(
            sql.down
                .contains("DROP CONSTRAINT \"widgets_maybe_signed_byte_check\""),
            "i8 CHECK rollback drops the named constraint: {}",
            sql.down
        );
    }

    #[test]
    fn alter_column_set_check_for_u32_bigint() {
        let sql = emit_alter_column(
            "widgets",
            "medium_count",
            &ColumnChange::SetCheck {
                from: None,
                to: Some("\"medium_count\" >= 0 AND \"medium_count\" <= 4294967295".to_string()),
            },
        );
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"widgets_medium_count_check\""),
            "u32 CHECK uses table+column constraint name: {}",
            sql.up
        );
        assert!(
            sql.up
                .contains("CHECK (\"medium_count\" >= 0 AND \"medium_count\" <= 4294967295)"),
            "u32 CHECK expression wraps the projected bound: {}",
            sql.up
        );
        assert!(
            sql.down
                .contains("DROP CONSTRAINT \"widgets_medium_count_check\""),
            "u32 CHECK rollback drops the named constraint: {}",
            sql.down
        );
    }

    #[test]
    fn alter_column_set_check_for_u64_numeric() {
        // Pre-wired for djogi#190 — once tokio-postgres bind/decode
        // shims land, the projection will start emitting this CHECK
        // for `u64` columns automatically. Pinning the SQL shape now
        // means the integer-widening contract from #186 stays intact
        // when #190 surfaces the new type.
        let sql = emit_alter_column(
            "widgets",
            "huge_count",
            &ColumnChange::SetCheck {
                from: None,
                to: Some(
                    "\"huge_count\" >= 0 AND \"huge_count\" <= 18446744073709551615".to_string(),
                ),
            },
        );
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"widgets_huge_count_check\""),
            "u64 CHECK uses table+column constraint name: {}",
            sql.up
        );
        assert!(
            sql.up
                .contains("CHECK (\"huge_count\" >= 0 AND \"huge_count\" <= 18446744073709551615)"),
            "u64 CHECK expression wraps the projected bound: {}",
            sql.up
        );
    }

    // ── djogi#187 — type-derived temporal year-bounds CHECKs ───────────────
    //
    // Mirrors `alter_column_set_check_uses_named_constraint` for the
    // temporal column types whose Rust source range (±9999 years for
    // `time::Date` / `time::OffsetDateTime`) is narrower than the
    // Postgres column type's range. The expression strings come from
    // `migrate::projection::field_type_check`; these tests pin the SQL
    // the emitter wraps around them. Together with the
    // `field_type_check_for_*` tests in `projection.rs` this covers
    // the full descriptor → SQL pipeline for the temporal arms.

    #[test]
    fn alter_column_set_check_for_time_date() {
        // One-sided upper-bound CHECK: `time::Date::MAX_YEAR = 9999`
        // is the effective protective bound (Postgres's date input
        // parser rejects everything below `time::Date::MIN_YEAR`
        // naturally). See `migrate::projection::field_type_check` doc
        // comment for the lower-bound rationale.
        let sql = emit_alter_column(
            "products",
            "launch_date",
            &ColumnChange::SetCheck {
                from: None,
                to: Some("\"launch_date\" <= DATE '9999-12-31'".to_string()),
            },
        );
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"products_launch_date_check\""),
            "Date CHECK uses table+column constraint name: {}",
            sql.up
        );
        assert!(
            sql.up
                .contains("CHECK (\"launch_date\" <= DATE '9999-12-31')"),
            "Date CHECK expression wraps the projected bound: {}",
            sql.up
        );
        assert!(
            sql.down
                .contains("DROP CONSTRAINT \"products_launch_date_check\""),
            "Date CHECK rollback drops the named constraint: {}",
            sql.down
        );
    }

    #[test]
    fn alter_column_set_check_for_time_offset_datetime() {
        let sql = emit_alter_column(
            "events",
            "occurred_at",
            &ColumnChange::SetCheck {
                from: None,
                to: Some(
                    "\"occurred_at\" <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00'".to_string(),
                ),
            },
        );
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"events_occurred_at_check\""),
            "Timestamptz CHECK uses table+column constraint name: {}",
            sql.up
        );
        assert!(
            sql.up
                .contains("CHECK (\"occurred_at\" <= TIMESTAMPTZ '9999-12-31 23:59:59.999999+00')"),
            "Timestamptz CHECK expression wraps the projected bound: {}",
            sql.up
        );
        assert!(
            sql.down
                .contains("DROP CONSTRAINT \"events_occurred_at_check\""),
            "Timestamptz CHECK rollback drops the named constraint: {}",
            sql.down
        );
    }

    #[test]
    fn alter_column_drop_check_emits_drop_constraint_only() {
        // The DROP scenario from the lifecycle contract: descriptor
        // evolves from u32 → i64. `field_type_check` returns `None` on
        // the new descriptor, the differ emits
        // `SetCheck { from: Some(prior), to: None }`, and the SQL
        // emitter produces a single DROP CONSTRAINT statement on the
        // up side. The down side restores the prior CHECK via
        // ADD CONSTRAINT — fully recoverable rollback (GPT-5.5 fix).
        let prior = "\"medium_count\" >= 0 AND \"medium_count\" <= 4294967295";
        let sql = emit_alter_column(
            "widgets",
            "medium_count",
            &ColumnChange::SetCheck {
                from: Some(prior.to_string()),
                to: None,
            },
        );
        assert!(
            sql.up
                .contains("DROP CONSTRAINT \"widgets_medium_count_check\""),
            "drop CHECK emits a named DROP CONSTRAINT: {}",
            sql.up
        );
        assert!(
            !sql.up.contains("ADD CONSTRAINT"),
            "drop CHECK must not also add a constraint on the up side: {}",
            sql.up
        );
        // GPT-5.5 fix: down side restores the prior CHECK losslessly.
        assert!(
            sql.down
                .contains("ADD CONSTRAINT \"widgets_medium_count_check\""),
            "drop CHECK rollback must ADD the prior constraint: {}",
            sql.down
        );
        assert!(
            sql.down.contains(&format!("CHECK ({prior})")),
            "drop CHECK rollback must restore the prior expression: {}",
            sql.down
        );
        assert!(
            sql.lossy.is_none(),
            "drop CHECK rollback is lossless when `from` is known: {:?}",
            sql.lossy
        );
    }

    #[test]
    fn alter_column_amend_check_pair_emits_drop_then_add() {
        // The AMEND scenario from the lifecycle contract: descriptor
        // evolves from u32 → u64 (or any CHECK expression change).
        // The differ at `migrate/diff.rs::emit_alter_column` emits two
        // ColumnChange entries in order — `SetCheck { from: Some(b), to: None }`
        // then `SetCheck { from: None, to: Some(a) }` — and the SQL
        // emitter renders them as a clean DROP-then-ADD pair against
        // the same constraint name slot. Without the differ's two-step
        // emission the second ALTER would collide on the existing
        // constraint name.
        //
        // This test simulates the SQL pair the emitter produces when
        // the differ supplies the two changes in order. Walk the two
        // emissions and verify their SQL forms compose correctly.
        let prior_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let new_expr = "\"amount\" >= 0 AND \"amount\" <= 18446744073709551615";
        let drop_sql = emit_alter_column(
            "widgets",
            "amount",
            &ColumnChange::SetCheck {
                from: Some(prior_expr.to_string()),
                to: None,
            },
        );
        let add_sql = emit_alter_column(
            "widgets",
            "amount",
            &ColumnChange::SetCheck {
                from: None,
                to: Some(new_expr.to_string()),
            },
        );
        // The first emission drops the existing constraint.
        assert!(
            drop_sql
                .up
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "AMEND step 1 drops the existing constraint: {}",
            drop_sql.up
        );
        // The second emission re-adds it under the same name with the
        // new expression. By the time this ALTER runs the previous DROP
        // has already cleared the slot, so Postgres accepts the ADD.
        assert!(
            add_sql
                .up
                .contains("ADD CONSTRAINT \"widgets_amount_check\""),
            "AMEND step 2 adds the new constraint under the same name: {}",
            add_sql.up
        );
        assert!(
            add_sql
                .up
                .contains("CHECK (\"amount\" >= 0 AND \"amount\" <= 18446744073709551615)"),
            "AMEND step 2 carries the new CHECK expression: {}",
            add_sql.up
        );
    }

    fn assert_type_change_check_sql_order(before_check: Option<&str>, after_check: Option<&str>) {
        let before = applied_schema_with_amount_check("INTEGER", before_check);
        let after = applied_schema_with_amount_check("BIGINT", after_check);
        let delta = crate::migrate::diff::diff_schemas(
            &before,
            &after,
            crate::migrate::BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
        );
        let statements = lower_delta(&delta).expect("lower delta");
        assert_eq!(statements.len(), 3, "expected 3 migration statements");
        // ── UP ordering: drop existing CHECK, alter type, add new CHECK.
        assert!(
            statements[0]
                .up
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "first statement must drop existing CHECK: {}",
            statements[0].up
        );
        assert!(
            statements[1]
                .up
                .contains("ALTER TABLE \"widgets\" ALTER COLUMN \"amount\" TYPE BIGINT"),
            "second statement must alter column type: {}",
            statements[1].up
        );
        assert!(
            statements[2]
                .up
                .contains("ADD CONSTRAINT \"widgets_amount_check\""),
            "third statement must add replacement CHECK: {}",
            statements[2].up
        );

        // ── DOWN ordering: the composed down file walks ops in reverse,
        // so the rollback order is statements[2].down, [1].down, [0].down.
        // GPT-5.5 fix: every step's down must restore the previous state.
        // This is the central assertion the lossy-rollback bug was missing.

        // statements[2] = add new CHECK; its rollback drops the new CHECK
        // (no prior to restore — the prior is restored by statements[0].down).
        assert!(
            statements[2]
                .down
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "third statement rollback must drop the new CHECK: {}",
            statements[2].down
        );

        // statements[1] = alter column TYPE; rollback reverts to INTEGER.
        assert!(
            statements[1]
                .down
                .contains("ALTER TABLE \"widgets\" ALTER COLUMN \"amount\" TYPE INTEGER"),
            "second statement rollback must revert type: {}",
            statements[1].down
        );

        // statements[0] = drop original CHECK; rollback re-adds it with
        // the original expression — the GPT-5.5 BLOCK fix in action.
        let before_expr = before_check.expect("before_check should be Some for this helper");
        assert!(
            statements[0]
                .down
                .contains("ADD CONSTRAINT \"widgets_amount_check\""),
            "first statement rollback must re-add the original CHECK: {}",
            statements[0].down
        );
        assert!(
            statements[0]
                .down
                .contains(&format!("CHECK ({before_expr})")),
            "first statement rollback must restore the original CHECK expression \
             `{before_expr}`, got: {}",
            statements[0].down
        );
        assert!(
            statements[0].lossy.is_none()
                && statements[1].lossy.is_none()
                && statements[2].lossy.is_none(),
            "type-change-with-CHECK rollback is now fully recoverable; lossy: \
             {:?} / {:?} / {:?}",
            statements[0].lossy,
            statements[1].lossy,
            statements[2].lossy,
        );
    }

    #[test]
    fn alter_column_type_change_orders_drop_then_add_for_unchanged_check() {
        assert_type_change_check_sql_order(
            Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"),
            Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"),
        );
    }

    #[test]
    fn alter_column_type_change_orders_drop_then_add_for_changed_check() {
        assert_type_change_check_sql_order(
            Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"),
            Some("\"amount\" >= 0 AND \"amount\" <= 18446744073709551615"),
        );
    }

    // ── GPT-5.5 BLOCK: CHECK rollback restoration (issue #105/#188) ──────
    //
    // The previous IR's `SetCheck(Option<String>)` carried only the
    // target check expression, so the down-side rollback for the DROP
    // arm could only emit a comment ("prior CHECK not recoverable").
    // For a type migration on a checked column, this left the column
    // unconstrained after rollback even though the prior expression was
    // structurally available in the differ. The refactor to
    // `SetCheck { from, to }` carries both, so the down-side restores
    // exactly. These tests pin that behaviour for the three lifecycle
    // shapes the differ produces.

    #[test]
    fn type_change_unchanged_check_down_restores_original_check() {
        // Type-only change with the same CHECK on both sides.
        // Down composes in reverse: drop new CHECK, alter type back,
        // ADD original CHECK. Asserts every step in
        // `assert_type_change_check_sql_order` for the unchanged-check case.
        assert_type_change_check_sql_order(
            Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"),
            Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"),
        );
    }

    #[test]
    fn type_change_changed_check_down_restores_old_not_new_check() {
        // Type change where the CHECK expression also changes.
        // The critical assertion: the down-side restores the OLD
        // check (`4294967295`), not the new check (`18446744073709551615`).
        // This is the exact case GPT-5.5 review flagged as BLOCK.
        let before_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let after_expr = "\"amount\" >= 0 AND \"amount\" <= 18446744073709551615";
        let before = applied_schema_with_amount_check("INTEGER", Some(before_expr));
        let after = applied_schema_with_amount_check("BIGINT", Some(after_expr));
        let delta = crate::migrate::diff::diff_schemas(
            &before,
            &after,
            crate::migrate::BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
        );
        let statements = lower_delta(&delta).expect("lower delta");
        // The first op's down side restores the OLD expression.
        assert!(
            statements[0]
                .down
                .contains(&format!("CHECK ({before_expr})")),
            "rollback of the DROP step must restore the OLD CHECK \
             ({before_expr}), got: {}",
            statements[0].down
        );
        assert!(
            !statements[0].down.contains(after_expr),
            "rollback of the DROP step must NOT contain the NEW CHECK \
             expression ({after_expr}); that would imply the rollback \
             left the new expression behind: {}",
            statements[0].down
        );
    }

    #[test]
    fn type_change_without_prior_check_has_no_check_steps() {
        // Sanity check: when the before-side has no CHECK, the differ
        // emits ONLY the type-change step (no SetCheck pair). Down
        // simply reverts the type — there's no CHECK to restore.
        let before = applied_schema_with_amount_check("INTEGER", None);
        let after = applied_schema_with_amount_check("BIGINT", None);
        let delta = crate::migrate::diff::diff_schemas(
            &before,
            &after,
            crate::migrate::BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
        );
        let statements = lower_delta(&delta).expect("lower delta");
        assert_eq!(statements.len(), 1, "type-only change emits one statement");
        assert!(
            statements[0]
                .up
                .contains("ALTER TABLE \"widgets\" ALTER COLUMN \"amount\" TYPE BIGINT"),
            "the single statement is the type change: {}",
            statements[0].up
        );
        assert!(
            statements[0]
                .down
                .contains("ALTER TABLE \"widgets\" ALTER COLUMN \"amount\" TYPE INTEGER"),
            "rollback reverts the type: {}",
            statements[0].down
        );
    }

    #[test]
    fn amend_check_only_down_restores_original_expression() {
        // CHECK expression change without a type change (AMEND-only).
        // Up: drop b, add a. Down (reversed): drop a, add b.
        // The composed down file rolls back to the original CHECK.
        let before_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let after_expr = "\"amount\" >= 0 AND \"amount\" <= 18446744073709551615";
        let before = applied_schema_with_amount_check("BIGINT", Some(before_expr));
        let after = applied_schema_with_amount_check("BIGINT", Some(after_expr));
        let delta = crate::migrate::diff::diff_schemas(
            &before,
            &after,
            crate::migrate::BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
        );
        let statements = lower_delta(&delta).expect("lower delta");
        assert_eq!(
            statements.len(),
            2,
            "AMEND emits two statements: {statements:?}"
        );

        // Up: step 0 drops old CHECK; step 1 adds new CHECK.
        assert!(
            statements[0]
                .up
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "up step 0: drop old CHECK: {}",
            statements[0].up
        );
        assert!(
            statements[1].up.contains(&format!(
                "ADD CONSTRAINT \"widgets_amount_check\" CHECK ({after_expr})"
            )),
            "up step 1: add new CHECK: {}",
            statements[1].up
        );

        // Down (the file walks ops in reverse):
        // step 1's down drops the new CHECK,
        // step 0's down re-adds the original CHECK with the OLD expression.
        assert!(
            statements[1]
                .down
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "down step 1 (composed first in reverse): drops the new CHECK: {}",
            statements[1].down
        );
        assert!(
            statements[0].down.contains(&format!(
                "ADD CONSTRAINT \"widgets_amount_check\" CHECK ({before_expr})"
            )),
            "down step 0 (composed last): restores the ORIGINAL CHECK ({before_expr}): {}",
            statements[0].down
        );
        assert!(
            statements[0].lossy.is_none() && statements[1].lossy.is_none(),
            "AMEND rollback is fully recoverable: {:?} / {:?}",
            statements[0].lossy,
            statements[1].lossy,
        );
    }

    #[test]
    fn pure_drop_check_down_restores_prior() {
        // Pure DROP (no type change): descriptor evolves from a CHECKed
        // column to no CHECK. Up: DROP CONSTRAINT. Down: ADD
        // CONSTRAINT with the prior expression (lossless rollback).
        let prior_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let before = applied_schema_with_amount_check("BIGINT", Some(prior_expr));
        let after = applied_schema_with_amount_check("BIGINT", None);
        let delta = crate::migrate::diff::diff_schemas(
            &before,
            &after,
            crate::migrate::BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
        );
        let statements = lower_delta(&delta).expect("lower delta");
        assert_eq!(statements.len(), 1, "pure DROP emits one statement");
        assert!(
            statements[0]
                .up
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "up: drop CHECK: {}",
            statements[0].up
        );
        assert!(
            statements[0].down.contains(&format!(
                "ADD CONSTRAINT \"widgets_amount_check\" CHECK ({prior_expr})"
            )),
            "down: restore prior CHECK losslessly: {}",
            statements[0].down
        );
        assert!(
            statements[0].lossy.is_none(),
            "pure DROP rollback is now lossless: {:?}",
            statements[0].lossy,
        );
    }

    #[test]
    fn pure_add_check_down_drops_without_residue() {
        // Pure ADD (no type change, no prior CHECK): descriptor evolves
        // from no CHECK to a CHECKed column. Up: ADD CONSTRAINT.
        // Down: DROP CONSTRAINT (no prior to restore).
        let new_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let before = applied_schema_with_amount_check("BIGINT", None);
        let after = applied_schema_with_amount_check("BIGINT", Some(new_expr));
        let delta = crate::migrate::diff::diff_schemas(
            &before,
            &after,
            crate::migrate::BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
        );
        let statements = lower_delta(&delta).expect("lower delta");
        assert_eq!(statements.len(), 1, "pure ADD emits one statement");
        assert!(
            statements[0].up.contains(&format!(
                "ADD CONSTRAINT \"widgets_amount_check\" CHECK ({new_expr})"
            )),
            "up: install CHECK: {}",
            statements[0].up
        );
        assert!(
            statements[0]
                .down
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "down: drop CHECK on rollback: {}",
            statements[0].down
        );
        assert!(
            !statements[0].down.contains("ADD CONSTRAINT"),
            "pure ADD rollback must not re-install anything: {}",
            statements[0].down
        );
    }

    #[test]
    fn amend_merged_form_renders_drop_then_add_on_both_sides() {
        // The structural completeness test for the SQL emitter's merged
        // AMEND arm — `SetCheck { from: Some, to: Some }`. The differ
        // splits AMEND across two entries today, but the emitter
        // handles the merged form too for future callers (e.g. a
        // higher-level optimizer that collapses adjacent SetCheck pairs).
        let sql = emit_alter_column(
            "widgets",
            "amount",
            &ColumnChange::SetCheck {
                from: Some("\"amount\" >= 0".to_string()),
                to: Some("\"amount\" > 0".to_string()),
            },
        );
        // Up: drop the prior, add the new (single emit, two ALTERs).
        assert!(
            sql.up.contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "merged AMEND up drops first: {}",
            sql.up
        );
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"widgets_amount_check\" CHECK (\"amount\" > 0)"),
            "merged AMEND up adds the new: {}",
            sql.up
        );
        // Down: drop the new, add back the prior.
        assert!(
            sql.down
                .contains("DROP CONSTRAINT \"widgets_amount_check\""),
            "merged AMEND down drops the new first: {}",
            sql.down
        );
        assert!(
            sql.down
                .contains("ADD CONSTRAINT \"widgets_amount_check\" CHECK (\"amount\" >= 0)"),
            "merged AMEND down restores the prior: {}",
            sql.down
        );
        assert!(sql.lossy.is_none(), "merged AMEND rollback is lossless");
    }

    #[test]
    fn alter_column_set_unique_uses_named_key_constraint() {
        let sql = emit_alter_column("users", "email", &ColumnChange::SetUnique(true));
        assert!(
            sql.up
                .contains("ADD CONSTRAINT \"users_email_key\" UNIQUE (\"email\")")
        );
        assert!(sql.down.contains("DROP CONSTRAINT \"users_email_key\""));
    }

    #[test]
    fn alter_column_set_indexed_emits_create_index() {
        let sql = emit_alter_column("users", "name", &ColumnChange::SetIndexed(true));
        assert!(sql.up.starts_with("CREATE INDEX"));
        assert!(sql.up.contains("\"users\""));
        assert!(sql.up.contains("(\"name\")"));
    }

    // ── SetIdentity (Codex T22 BLOCK-3) ──────────────────────────────────────

    #[test]
    fn alter_column_set_identity_add_emits_add_generated_clause() {
        // Pre-fix snapshot: identity = None. Fresh projection:
        // identity = Some(ByDefault). Differ emits ADD GENERATED
        // followed by a setval that syncs the new sequence to
        // MAX(id) + 1 — Codex T22 round-3 BLOCK-2: without this,
        // populated tables collide on the next default-id INSERT.
        use crate::migrate::schema::IdentityKindSchema;
        let sql = emit_alter_column(
            "countries",
            "id",
            &ColumnChange::SetIdentity {
                from: None,
                to: Some(IdentityKindSchema::ByDefault),
            },
        );
        assert_eq!(
            sql.up,
            "ALTER TABLE \"countries\" ALTER COLUMN \"id\" ADD GENERATED BY DEFAULT AS IDENTITY;\n\
             SELECT setval(pg_get_serial_sequence('countries', 'id'), \
             GREATEST(COALESCE((SELECT MAX(\"id\") FROM \"countries\"), 0), 0) + 1, false);"
        );
        assert_eq!(
            sql.down,
            "ALTER TABLE \"countries\" ALTER COLUMN \"id\" DROP IDENTITY;"
        );
    }

    #[test]
    fn alter_column_set_identity_drop_emits_drop_identity() {
        // Reverse direction — adopter intentionally removes IDENTITY.
        // The down direction adds it back, with the same setval
        // sync the up-add path emits (the collision risk is
        // direction-agnostic — adding identity to a populated
        // table needs the sequence synced regardless of which
        // direction the migration came from).
        use crate::migrate::schema::IdentityKindSchema;
        let sql = emit_alter_column(
            "countries",
            "id",
            &ColumnChange::SetIdentity {
                from: Some(IdentityKindSchema::ByDefault),
                to: None,
            },
        );
        assert_eq!(
            sql.up,
            "ALTER TABLE \"countries\" ALTER COLUMN \"id\" DROP IDENTITY;"
        );
        assert_eq!(
            sql.down,
            "ALTER TABLE \"countries\" ALTER COLUMN \"id\" ADD GENERATED BY DEFAULT AS IDENTITY;\n\
             SELECT setval(pg_get_serial_sequence('countries', 'id'), \
             GREATEST(COALESCE((SELECT MAX(\"id\") FROM \"countries\"), 0), 0) + 1, false);"
        );
    }

    #[test]
    fn alter_column_set_identity_kind_change_emits_set_generated() {
        // BY DEFAULT ↔ ALWAYS — preserves the existing sequence,
        // changes only the kind (SET GENERATED <kind> syntax).
        use crate::migrate::schema::IdentityKindSchema;
        let sql = emit_alter_column(
            "countries",
            "id",
            &ColumnChange::SetIdentity {
                from: Some(IdentityKindSchema::ByDefault),
                to: Some(IdentityKindSchema::Always),
            },
        );
        assert_eq!(
            sql.up,
            "ALTER TABLE \"countries\" ALTER COLUMN \"id\" SET GENERATED ALWAYS;"
        );
        assert_eq!(
            sql.down,
            "ALTER TABLE \"countries\" ALTER COLUMN \"id\" SET GENERATED BY DEFAULT;"
        );
    }

    // ── Foreign keys ───────────────────────────────────────────────────

    #[test]
    fn add_foreign_key_emits_named_constraint_with_default_restrict() {
        let sql = emit_add_foreign_key(
            "posts",
            "author_id",
            &ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            },
        );
        assert!(sql.up.contains("ADD CONSTRAINT \"posts_author_id_fkey\""));
        assert!(sql.up.contains("FOREIGN KEY (\"author_id\")"));
        assert!(sql.up.contains("REFERENCES \"users\" (\"id\")"));
        assert!(
            sql.up.contains("ON DELETE RESTRICT"),
            "default cascade must round-trip as RESTRICT, got: {}",
            sql.up
        );
    }

    #[test]
    fn add_foreign_key_propagates_cascade_kind() {
        // Codex T3 review B-3: AddForeignKey must NOT silently rewrite
        // the declared cascade as RESTRICT. Round-trip every variant.
        for (cascade, expected) in [
            (OnDeleteSchema::Restrict, "ON DELETE RESTRICT"),
            (OnDeleteSchema::Cascade, "ON DELETE CASCADE"),
            (OnDeleteSchema::SetNull, "ON DELETE SET NULL"),
            (OnDeleteSchema::SetDefault, "ON DELETE SET DEFAULT"),
            (OnDeleteSchema::NoAction, "ON DELETE NO ACTION"),
        ] {
            let sql = emit_add_foreign_key(
                "posts",
                "author_id",
                &ForeignKeySchema {
                    deferrable: false,
                    initially_deferred: false,
                    on_delete: cascade,
                    ref_column: "id".to_string(),
                    ref_table: "users".to_string(),
                },
            );
            assert!(
                sql.up.contains(expected),
                "cascade {cascade:?} must emit `{expected}`; got: {}",
                sql.up
            );
        }
    }

    #[test]
    fn render_alter_table_add_deferrable_fk() {
        let sql = emit_add_foreign_key(
            "posts",
            "author_id",
            &ForeignKeySchema {
                deferrable: true,
                initially_deferred: true,
                on_delete: OnDeleteSchema::Cascade,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            },
        );
        assert_eq!(
            sql.up,
            "ALTER TABLE \"posts\" ADD CONSTRAINT \"posts_author_id_fkey\" \
             FOREIGN KEY (\"author_id\") REFERENCES \"users\" (\"id\") \
             ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED;"
        );
    }

    #[test]
    fn drop_foreign_key_rollback_recreates_constraint_with_cascade() {
        // Codex T3 review B-3: DropForeignKey now carries the full
        // ForeignKeySchema so the rollback recreates the FK with the
        // original `ON DELETE ...` clause — no comment-only down side.
        let sql = emit_drop_foreign_key(
            "posts",
            "author_id",
            &ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Cascade,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            },
        );
        assert!(sql.up.contains("DROP CONSTRAINT \"posts_author_id_fkey\""));
        assert!(
            sql.down.contains("ADD CONSTRAINT \"posts_author_id_fkey\""),
            "rollback must recreate the constraint, got: {}",
            sql.down
        );
        assert!(
            sql.down.contains("REFERENCES \"users\" (\"id\")"),
            "rollback must restore the target, got: {}",
            sql.down
        );
        assert!(
            sql.down.contains("ON DELETE CASCADE"),
            "rollback must restore the cascade, got: {}",
            sql.down
        );
        // Now lossless — no warning needed because the diff carries
        // the full FK shape.
        assert!(
            sql.lossy.is_none(),
            "DropForeignKey rollback is structurally clean now; no lossy marker expected"
        );
    }

    // ── Indexes ────────────────────────────────────────────────────────

    #[test]
    fn add_index_basic_emits_create_index() {
        let i = idx("users_name_idx", "users", &["name"]);
        let sql = emit_add_index(&i);
        assert_eq!(
            sql.up,
            "CREATE INDEX \"users_name_idx\" ON \"users\" USING btree (\"name\");"
        );
        assert_eq!(sql.down, "DROP INDEX \"users_name_idx\";");
    }

    #[test]
    fn add_index_concurrently_marks_out_of_transaction() {
        let mut i = idx("events_ts_idx", "events", &["ts"]);
        i.requires_out_of_transaction = true;
        let sql = emit_add_index(&i);
        assert!(sql.up.contains("CREATE INDEX CONCURRENTLY"));
        assert!(sql.down.contains("DROP INDEX CONCURRENTLY"));
    }

    #[test]
    fn add_unique_index_uses_unique_keyword() {
        let mut i = idx("users_email_uidx", "users", &["email"]);
        i.kind = IndexKindSchema::UniqueIndex;
        let sql = emit_add_index(&i);
        assert!(sql.up.starts_with("CREATE UNIQUE INDEX"));
    }

    #[test]
    fn add_index_with_include_emits_include_clause() {
        let mut i = idx("users_email_uidx", "users", &["email"]);
        i.kind = IndexKindSchema::UniqueIndex;
        i.include = vec!["tenant_id".to_string()];
        let sql = emit_add_index(&i);
        assert!(sql.up.contains("INCLUDE (\"tenant_id\")"));
    }

    #[test]
    fn add_index_with_predicate_emits_where_clause() {
        let mut i = idx("users_email_uidx", "users", &["email"]);
        i.kind = IndexKindSchema::UniqueIndex;
        i.predicate = Some("deleted_at IS NULL".to_string());
        let sql = emit_add_index(&i);
        assert!(sql.up.contains("WHERE deleted_at IS NULL"));
    }

    #[test]
    fn add_index_preserves_composite_column_order() {
        let i = idx("orgs_a_b_idx", "orgs", &["b_first", "a_second"]);
        let sql = emit_add_index(&i);
        // Order is `b_first, a_second` because that's how the diff
        // listed them. Sorting would change semantics.
        let bpos = sql.up.find("\"b_first\"").expect("b_first");
        let apos = sql.up.find("\"a_second\"").expect("a_second");
        assert!(bpos < apos);
    }

    #[test]
    fn add_index_expression_target_uses_double_parens() {
        let i = IndexSchema {
            target: IndexTargetSchema::Expression("lower(email)".to_string()),
            ..idx("users_lower_email_idx", "users", &["x"])
        };
        let sql = emit_add_index(&i);
        assert!(sql.up.contains("((lower(email)))"));
    }

    #[test]
    fn add_index_per_column_order_and_nulls_emitted() {
        let target = IndexTargetSchema::Columns(vec![IndexColumnSchema {
            name: "ts".to_string(),
            nulls: IndexNullsOrderSchema::Last,
            opclass: None,
            order: IndexOrderSchema::Desc,
        }]);
        let i = IndexSchema {
            target,
            ..idx("events_ts_idx", "events", &["ts"])
        };
        let sql = emit_add_index(&i);
        assert!(sql.up.contains("\"ts\" DESC NULLS LAST"));
    }

    #[test]
    fn add_index_nulls_not_distinct_emits_clause() {
        let mut i = idx("users_email_uidx", "users", &["email"]);
        i.kind = IndexKindSchema::UniqueIndex;
        i.nulls_not_distinct = true;
        let sql = emit_add_index(&i);
        assert!(sql.up.contains("NULLS NOT DISTINCT"));
    }

    #[test]
    fn drop_index_recreates_in_down_with_full_metadata() {
        let mut i = idx("users_email_uidx", "users", &["email"]);
        i.kind = IndexKindSchema::UniqueIndex;
        i.predicate = Some("deleted_at IS NULL".to_string());
        let sql = emit_drop_index(&i);
        assert!(sql.up.starts_with("DROP INDEX"));
        // Down side rebuilds using the carried IndexSchema.
        assert!(sql.down.contains("CREATE UNIQUE INDEX"));
        assert!(sql.down.contains("WHERE deleted_at IS NULL"));
        // Lossy marker still surfaces (rebuild time is the warning).
        assert!(matches!(
            sql.lossy.as_ref().map(|w| w.kind),
            Some(LossyRollbackKind::DropIndex)
        ));
    }

    // ── Enums ──────────────────────────────────────────────────────────

    #[test]
    fn add_enum_emits_create_type_with_quoted_variants() {
        let e = EnumSchema {
            name: "status".to_string(),
            variants: vec!["active".to_string(), "deleted".to_string()],
        };
        let sql = emit_add_enum(&e);
        assert_eq!(
            sql.up,
            "CREATE TYPE \"status\" AS ENUM ('active', 'deleted');"
        );
        assert_eq!(sql.down, "DROP TYPE \"status\";");
    }

    #[test]
    fn add_enum_variant_with_apostrophe_quotes_correctly() {
        let e = EnumSchema {
            name: "title".to_string(),
            variants: vec!["it's complicated".to_string()],
        };
        let sql = emit_add_enum(&e);
        assert!(sql.up.contains("'it''s complicated'"));
    }

    #[test]
    fn drop_enum_marks_lossy() {
        let sql = emit_drop_enum("status");
        assert_eq!(sql.up, "DROP TYPE \"status\";");
        assert!(sql.down.contains("LOSSY ROLLBACK"));
        assert!(matches!(
            sql.lossy.as_ref().map(|w| w.kind),
            Some(LossyRollbackKind::DropEnum)
        ));
    }

    #[test]
    fn add_enum_variant_without_anchor_appends() {
        // Codex T3 review B-2: when the differ supplies no anchor,
        // emit a tail-append (no positional clause).
        let sql = emit_add_enum_variant("status", "archived", None);
        assert_eq!(sql.up, "ALTER TYPE \"status\" ADD VALUE 'archived';");
        // Postgres has no DROP VALUE — rollback is lossy.
        assert!(sql.down.contains("no `ALTER TYPE ... DROP VALUE`"));
    }

    #[test]
    fn add_enum_variant_with_before_anchor_emits_before_clause() {
        // Codex T3 review B-2: an anchor in `Before` direction must
        // produce `ALTER TYPE ... ADD VALUE 'new' BEFORE 'anchor';`.
        let anchor = EnumVariantAnchor {
            variant: "deleted".to_string(),
            kind: EnumVariantAnchorKind::Before,
        };
        let sql = emit_add_enum_variant("status", "archived", Some(&anchor));
        assert_eq!(
            sql.up,
            "ALTER TYPE \"status\" ADD VALUE 'archived' BEFORE 'deleted';"
        );
    }

    #[test]
    fn add_enum_variant_with_after_anchor_emits_after_clause() {
        let anchor = EnumVariantAnchor {
            variant: "active".to_string(),
            kind: EnumVariantAnchorKind::After,
        };
        let sql = emit_add_enum_variant("status", "archived", Some(&anchor));
        assert_eq!(
            sql.up,
            "ALTER TYPE \"status\" ADD VALUE 'archived' AFTER 'active';"
        );
    }

    #[test]
    fn add_enum_variant_with_apostrophe_anchor_quotes_correctly() {
        // Anchor variant strings go through quote_string_literal so
        // embedded `'` doubles correctly.
        let anchor = EnumVariantAnchor {
            variant: "it's complicated".to_string(),
            kind: EnumVariantAnchorKind::Before,
        };
        let sql = emit_add_enum_variant("title", "settled", Some(&anchor));
        assert!(
            sql.up.contains("BEFORE 'it''s complicated'"),
            "got: {}",
            sql.up
        );
    }

    // ── Routing errors ─────────────────────────────────────────────────

    #[test]
    fn pk_type_flip_routes_to_t9_error() {
        let op = SchemaOperation::PkTypeFlip {
            table: "users".to_string(),
            from: PkKindSchema::HeerId,
            to: PkKindSchema::HeerIdRecencyBiased,
        };
        let err = lower_operation(&op).expect_err("PkTypeFlip must error");
        assert!(matches!(err, SqlEmitError::PkTypeFlipMustRouteToT9 { .. }));
    }

    #[test]
    fn unsupported_operation_propagates_reason() {
        let op = SchemaOperation::Unsupported {
            reason: "partition method change".to_string(),
        };
        let err = lower_operation(&op).expect_err("Unsupported must error");
        match err {
            SqlEmitError::Unsupported { reason } => {
                assert_eq!(reason, "partition method change");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    // ── Whole-delta façade ─────────────────────────────────────────────

    #[test]
    fn lower_delta_returns_empty_for_noop() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
            operations: Vec::new(),
            classification: Classification::NoOp,
        };
        let out = lower_delta(&delta).expect("ok");
        assert!(out.is_empty());
    }

    #[test]
    fn lower_delta_propagates_unsupported_as_hard_error() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
            operations: vec![SchemaOperation::Unsupported {
                reason: "X".to_string(),
            }],
            classification: Classification::Unsupported {
                reason: "X".to_string(),
            },
        };
        let err = lower_delta(&delta).expect_err("must error");
        assert!(matches!(err, SqlEmitError::Unsupported { .. }));
    }

    #[test]
    fn lower_delta_propagates_pk_flip_as_hard_error() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
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
        let err = lower_delta(&delta).expect_err("must error");
        assert!(matches!(err, SqlEmitError::PkTypeFlipMustRouteToT9 { .. }));
    }

    // ── Determinism ────────────────────────────────────────────────────

    #[test]
    fn same_delta_lowers_byte_identically() {
        let mut t = synth_table("users");
        t.columns.push(col("email", "TEXT", false));
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "".to_string(),
            },
            operations: vec![
                SchemaOperation::AddTable(t.clone()),
                SchemaOperation::AddIndex(idx("users_email_idx", "users", &["email"])),
            ],
            classification: Classification::Additive,
        };
        let a = lower_delta(&delta).unwrap();
        let b = lower_delta(&delta).unwrap();
        assert_eq!(a, b);
    }

    // ── Constraint-name truncation ─────────────────────────────────────

    #[test]
    fn truncate_constraint_handles_long_names_with_digest() {
        let long = format!("{}_a_b_c_d_e_f_g_h_i_j_check", "a".repeat(60));
        let truncated = truncate_constraint(long.clone());
        assert!(truncated.len() <= 63);
        // Same input -> same output (determinism).
        assert_eq!(truncate_constraint(long), truncated);
    }

    #[test]
    fn truncate_constraint_passes_short_names_through() {
        let short = "users_email_check".to_string();
        assert_eq!(truncate_constraint(short.clone()), short);
    }

    // ── Phase 7.5 PR 7: EXCLUSION + stored-generated emission ─────────

    use crate::migrate::schema::{
        ExclusionConstraintSchema, ExclusionElementSchema, GeneratedColumnSchema,
    };

    fn period_overlap_exclusion() -> ExclusionConstraintSchema {
        ExclusionConstraintSchema {
            deferrable: false,
            elements: vec![
                ExclusionElementSchema {
                    expr: "room_id".to_string(),
                    with_operator: "=".to_string(),
                },
                ExclusionElementSchema {
                    expr: "period".to_string(),
                    with_operator: "&&".to_string(),
                },
            ],
            initially_deferred: false,
            name: "no_overlap".to_string(),
            using: "gist".to_string(),
            where_clause: None,
        }
    }

    #[test]
    fn create_table_inlines_exclusion_constraint() {
        let mut t = synth_table("bookings");
        t.exclusion_constraints = vec![period_overlap_exclusion()];
        let op = emit_add_table(&t);
        assert!(op.up.contains(
            "CONSTRAINT \"no_overlap\" EXCLUDE USING gist (room_id WITH =, period WITH &&)"
        ));
        // Ensure the constraint is inside the parens (precedes ');').
        let close_paren = op.up.find(");").expect("expected closing paren");
        let constraint_idx = op.up.find("CONSTRAINT \"no_overlap\"").unwrap();
        assert!(constraint_idx < close_paren);
    }

    #[test]
    fn create_table_emits_exclusion_with_where_and_deferrable() {
        let mut t = synth_table("bookings");
        let mut excl = period_overlap_exclusion();
        excl.where_clause = Some("status = 'confirmed'".to_string());
        excl.deferrable = true;
        excl.initially_deferred = true;
        t.exclusion_constraints = vec![excl];
        let op = emit_add_table(&t);
        assert!(
            op.up.contains("WHERE (status = 'confirmed')"),
            "missing WHERE clause: {}",
            op.up
        );
        assert!(
            op.up.contains("DEFERRABLE INITIALLY DEFERRED"),
            "missing deferrable suffix: {}",
            op.up
        );
    }

    #[test]
    fn alter_table_add_exclusion_emits_constraint() {
        let op = emit_add_exclusion_constraint("bookings", &period_overlap_exclusion());
        assert_eq!(
            op.up,
            "ALTER TABLE \"bookings\" ADD CONSTRAINT \"no_overlap\" \
             EXCLUDE USING gist (room_id WITH =, period WITH &&);",
        );
        assert_eq!(
            op.down,
            "ALTER TABLE \"bookings\" DROP CONSTRAINT \"no_overlap\";",
        );
    }

    #[test]
    fn alter_table_drop_exclusion_round_trips_via_carried_schema() {
        let op =
            emit_drop_exclusion_constraint("bookings", "no_overlap", &period_overlap_exclusion());
        assert_eq!(
            op.up,
            "ALTER TABLE \"bookings\" DROP CONSTRAINT \"no_overlap\";",
        );
        // Down side reconstructs the EXCLUDE clause from the carried
        // schema — full round-trip without re-walking the descriptor.
        assert!(op.down.contains(
            "ADD CONSTRAINT \"no_overlap\" EXCLUDE USING gist (room_id WITH =, period WITH &&)"
        ));
    }

    #[test]
    fn add_column_with_generated_emits_stored_clause() {
        let column = ColumnSchema {
            generated: Some(GeneratedColumnSchema {
                expression: "LOWER(email)".to_string(),
                stored: true,
            }),
            ..col("email_lower", "TEXT", true)
        };
        let op = emit_add_column("users", &column);
        assert!(
            op.up.contains("GENERATED ALWAYS AS (LOWER(email)) STORED"),
            "missing GENERATED clause: {}",
            op.up
        );
        // Generated columns must NOT carry a DEFAULT clause —
        // Postgres rejects both on the same column.
        assert!(
            !op.up.contains("DEFAULT"),
            "generated column should not emit DEFAULT: {}",
            op.up
        );
    }

    #[test]
    fn create_table_inlines_generated_column() {
        let generated = ColumnSchema {
            generated: Some(GeneratedColumnSchema {
                expression: "LOWER(email)".to_string(),
                stored: true,
            }),
            ..col("email_lower", "TEXT", true)
        };
        let mut t = synth_table("users");
        t.columns.push(generated);
        let op = emit_add_table(&t);
        assert!(
            op.up
                .contains("\"email_lower\" TEXT GENERATED ALWAYS AS (LOWER(email)) STORED"),
            "missing inline GENERATED: {}",
            op.up
        );
    }

    #[test]
    fn add_table_emits_ddl_metadata_after_create_table() {
        let mut t = synth_table("widgets");
        t.table_comment = Some("Widget owner's table".to_string());
        t.storage_params = Some("fillfactor=70, autovacuum_enabled=false".to_string());
        t.tablespace = Some("fastspace".to_string());
        t.columns[1].comment = Some("Human-readable widget name".to_string());

        let sql = emit_add_table(&t);

        assert!(sql.up.contains("CREATE TABLE \"widgets\""));
        assert!(
            sql.up
                .contains("COMMENT ON TABLE \"widgets\" IS E'Widget owner''s table';")
        );
        assert!(
            sql.up.contains(
                "COMMENT ON COLUMN \"widgets\".\"name\" IS E'Human-readable widget name';"
            )
        );
        assert!(
            sql.up
                .contains("ALTER TABLE \"widgets\" SET (fillfactor=70, autovacuum_enabled=false);")
        );
        assert!(
            sql.up
                .contains("ALTER TABLE \"widgets\" SET TABLESPACE \"fastspace\";")
        );
    }

    #[test]
    fn table_comment_literal_escapes_backslash_quote_injection_fragments() {
        let dangerous = r"ok\'; DROP TABLE audit_log; --";

        let op = lower_operation(&SchemaOperation::SetTableComment {
            table: "widgets".to_string(),
            from: None,
            to: Some(dangerous.to_string()),
        })
        .expect("table comment lower");

        assert_eq!(
            op.up,
            r#"COMMENT ON TABLE "widgets" IS E'ok\\''; DROP TABLE audit_log; --';"#
        );
        assert_eq!(op.down, r#"COMMENT ON TABLE "widgets" IS NULL;"#);
    }

    #[test]
    fn column_comment_literal_escapes_backslash_quote_injection_fragments() {
        let dangerous = r"ok\'; DROP TABLE audit_log; -- owner's note";

        let op = emit_alter_column(
            "widgets",
            "name",
            &ColumnChange::SetComment {
                from: None,
                to: Some(dangerous.to_string()),
            },
        );

        assert_eq!(
            op.up,
            r#"COMMENT ON COLUMN "widgets"."name" IS E'ok\\''; DROP TABLE audit_log; -- owner''s note';"#
        );
        assert_eq!(op.down, r#"COMMENT ON COLUMN "widgets"."name" IS NULL;"#);
    }

    #[test]
    fn table_metadata_operations_are_reversible_sql() {
        let storage = lower_operation(&SchemaOperation::SetStorageParams {
            table: "widgets".to_string(),
            from: Some("fillfactor=80".to_string()),
            to: Some("fillfactor=70, autovacuum_enabled=false".to_string()),
        })
        .expect("storage params lower");
        assert_eq!(
            storage.up,
            "ALTER TABLE \"widgets\" RESET (fillfactor);\n\
             ALTER TABLE \"widgets\" SET (fillfactor=70, autovacuum_enabled=false);"
        );
        assert_eq!(
            storage.down,
            "ALTER TABLE \"widgets\" RESET (fillfactor, autovacuum_enabled);\n\
             ALTER TABLE \"widgets\" SET (fillfactor=80);"
        );

        let tablespace = lower_operation(&SchemaOperation::SetTablespace {
            table: "widgets".to_string(),
            from: None,
            to: Some("fastspace".to_string()),
        })
        .expect("tablespace lower");
        assert_eq!(
            tablespace.up,
            "ALTER TABLE \"widgets\" SET TABLESPACE \"fastspace\";"
        );
        assert_eq!(
            tablespace.down,
            "ALTER TABLE \"widgets\" SET TABLESPACE \"pg_default\";"
        );
    }

    #[test]
    fn storage_params_sql_emitter_rejects_injection_fragments() {
        for params in [
            "fillfactor=70); DROP TABLE x; --",
            "fillfactor=70--comment",
            "fillfactor=70/*comment*/",
            "fillfactor=(70)",
            "fillfactor=DROP",
        ] {
            let err = lower_operation(&SchemaOperation::SetStorageParams {
                table: "widgets".to_string(),
                from: None,
                to: Some(params.to_string()),
            })
            .expect_err("storage params injection fragment rejected");

            assert!(
                err.to_string().contains("storage_params"),
                "diagnostic names storage_params for {params:?}: {err}"
            );
        }
    }

    #[test]
    fn storage_params_sql_emitter_rejects_duplicate_keys_after_normalization() {
        let err = lower_operation(&SchemaOperation::SetStorageParams {
            table: "widgets".to_string(),
            from: None,
            to: Some("fillfactor=70, FillFactor=80".to_string()),
        })
        .expect_err("duplicate storage params key rejected");

        assert!(
            err.to_string().contains("duplicate"),
            "diagnostic mentions duplicate key: {err}"
        );
    }

    #[test]
    fn storage_params_sql_emitter_renders_from_parsed_entries() {
        let storage = lower_operation(&SchemaOperation::SetStorageParams {
            table: "widgets".to_string(),
            from: Some("FillFactor = 80, autovacuum_enabled = true".to_string()),
            to: Some("fillfactor = 70".to_string()),
        })
        .expect("storage params lower");

        assert_eq!(
            storage.up,
            "ALTER TABLE \"widgets\" RESET (fillfactor, autovacuum_enabled);\n\
             ALTER TABLE \"widgets\" SET (fillfactor=70);"
        );
        assert_eq!(
            storage.down,
            "ALTER TABLE \"widgets\" RESET (fillfactor);\n\
             ALTER TABLE \"widgets\" SET (fillfactor=80, autovacuum_enabled=true);"
        );
    }

    #[test]
    fn add_table_rejects_invalid_storage_params() {
        let mut t = synth_table("widgets");
        t.storage_params = Some("fillfactor=70); DROP TABLE x; --".to_string());

        let err = lower_operation(&SchemaOperation::AddTable(t))
            .expect_err("add table storage params injection rejected");

        assert!(
            err.to_string().contains("storage_params"),
            "diagnostic names storage_params: {err}"
        );
    }
}
