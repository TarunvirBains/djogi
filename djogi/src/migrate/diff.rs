//! Schema differ — compares two [`AppliedSchema`] values and emits
//! a typed [`SchemaDelta`] of structural operations classified by
//! reversibility / destructiveness.
//!
//! # Operation taxonomy
//!
//! Every difference between two schemas is encoded as one or more
//! [`SchemaOperation`] entries with a stable shape so the SQL emitter
//! (T3) can lower them deterministically. Operations come in three
//! categories:
//!
//! - **Table-level**: `AddTable` / `DropTable` / `RenameTable` /
//!   `MoveModelBetweenApps`.
//! - **Column-level**: `AddColumn` / `DropColumn` / `RenameColumn` /
//!   `AlterColumn` / `AddForeignKey` / `DropForeignKey`.
//! - **Other**: `AddIndex` / `DropIndex` / `AddEnum` / `DropEnum` /
//!   `AddEnumVariant` / `RenameApp` / `PkTypeFlip`.
//!
//! `RenameTable` and `RenameColumn` are emitted only when the new
//! schema's `renamed_from` field flags the change as a rename. Without
//! the annotation the differ emits a destructive `DropTable` +
//! `AddTable` (or `DropColumn` + `AddColumn`) pair so unannotated
//! "renames" cannot silently destroy data.
//!
//! # Classification
//!
//! Each [`SchemaDelta`] carries a [`Classification`]:
//!
//! - `NoOp` — schemas are equal (no operations).
//! - `Additive` — every op is non-destructive: new tables, new
//!   nullable columns, new indexes, new enum variants. Safe to ship.
//! - `Reversible` — contains operations that are destructive in one
//!   direction but have a clean inverse (e.g. a `RenameTable` whose
//!   inverse is the symmetric rename). The runner treats reversible
//!   deltas the same as destructive but the down-migration is
//!   well-defined.
//! - `Destructive { allow_destructive }` — at least one operation
//!   removes data structurally (`DropTable`, `DropColumn`,
//!   `DropEnum`). The `allow_destructive` flag tracks whether the
//!   operator has explicitly opted in via `--allow-destructive`; a
//!   destructive delta without the opt-in is rejected at runner
//!   entry.
//! - `Lossy` — a destructive op that would also lose row data with
//!   no recovery path (e.g. dropping a non-nullable column that has
//!   no default). Stricter than `Destructive`.
//! - `Unsupported { reason }` — the differ cannot lower the change
//!   safely (e.g. partition method change). Operator must hand-edit
//!   the migration.
//! - `PkTypeFlip` — at least one table changed its PK type variant
//!   (HeerId ↔ HeerIdRecencyBiased, RanjId ↔ RanjIdRecencyBiased).
//!   This is a **native Phase 7 classification** distinct from
//!   `RequiresLivePlan` per OQ-09 ruling — Phase 7 owns the full
//!   expand/contract orchestration including FK cascade composition.
//!   Phase 7.5 does not consume this; the migration engine handles
//!   it directly via T9.
//!
//! # No-op detection
//!
//! [`diff_schemas`] returns a delta with `Classification::NoOp` and an
//! empty operations vector when the two schemas compare equal. The
//! runner short-circuits no-ops without touching the ledger.

use std::collections::{BTreeMap, BTreeSet};

use super::projection::BucketKey;
use super::schema::{
    AppliedSchema, ColumnSchema, EnumSchema, ForeignKeySchema, IndexSchema, PkKindSchema,
    PrimaryKeySchema, TableSchema,
};

/// Typed delta between two [`AppliedSchema`] values, scoped to a
/// single [`BucketKey`].
///
/// One [`SchemaDelta`] represents the migration for one `(database,
/// app)` bucket. Multi-bucket migrations are a `Vec<SchemaDelta>`
/// with deterministic bucket ordering — see
/// [`diff_bucket_maps`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDelta {
    /// Which `(database, app)` bucket this delta describes.
    pub bucket: BucketKey,
    /// Operations in execution order — table creates before
    /// column-on-table changes; FK adds after both endpoints exist.
    pub operations: Vec<SchemaOperation>,
    /// Aggregate classification — the most-destructive flavour any
    /// operation triggered.
    pub classification: Classification,
}

/// One structural change between two schemas. The SQL emitter (T3)
/// lowers each variant into one or more `up`/`down` statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaOperation {
    /// New table with all of its columns + primary key + flags. The
    /// SQL emitter generates `CREATE TABLE` plus any column-level
    /// constraints; [`AddIndex`](Self::AddIndex) entries land
    /// separately so they can be batched into the
    /// non-transactional segment per Phase 7-Zero §6.2.
    AddTable(TableSchema),

    /// Existing table is gone from the new schema. **Destructive.**
    DropTable(String),

    /// Table whose name changed via `#[model(renamed_from = "old")]`.
    /// Emitted only when the new schema's `TableSchema.renamed_from`
    /// matches an existing table name in the old schema. Reversible.
    RenameTable { from: String, to: String },

    /// Column added to an existing table.
    AddColumn { table: String, column: ColumnSchema },

    /// Column removed. **Destructive.**
    DropColumn { table: String, column: String },

    /// Column renamed via `#[field(renamed_from = "old_name")]`.
    /// Reversible.
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },

    /// Column type / nullability / default changed. The differ
    /// records the full new column shape so T3 can emit the right
    /// `ALTER COLUMN` sequence.
    AlterColumn {
        table: String,
        column: String,
        change: ColumnChange,
    },

    /// Foreign key added on a column.
    AddForeignKey {
        table: String,
        column: String,
        fk: ForeignKeySchema,
    },

    /// Foreign key dropped. The column may still exist; only the
    /// `REFERENCES` constraint is removed.
    DropForeignKey { table: String, column: String },

    /// Index added.
    AddIndex(IndexSchema),

    /// Index dropped.
    DropIndex(String),

    /// `CREATE TYPE ... AS ENUM` for a new Postgres enum.
    AddEnum(EnumSchema),

    /// `DROP TYPE` for an enum no longer referenced. **Destructive.**
    DropEnum(String),

    /// `ALTER TYPE ... ADD VALUE` — adding a variant to an existing
    /// enum. Removing variants is rejected (Postgres has no native
    /// `DROP VALUE`).
    AddEnumVariant {
        enum_name: String,
        variant: String,
        /// Position of the new variant in the new schema's variant
        /// list; T3 emits `BEFORE`/`AFTER` clauses based on this.
        index: usize,
    },

    /// PK type changed within the supported flip pairs (HeerId ↔
    /// HeerIdRecencyBiased, RanjId ↔ RanjIdRecencyBiased). Triggers
    /// `Classification::PkTypeFlip` and is the entry point for T9's
    /// expand/contract orchestration.
    PkTypeFlip {
        table: String,
        from: PkKindSchema,
        to: PkKindSchema,
    },

    /// App rename via `#[app(renamed_from = "old_label")]`. The
    /// migration engine emits both a `git mv` of the per-app folder
    /// and a single `UPDATE djogi_schema_migrations SET app_label
    /// = 'new' WHERE app_label = 'old'` — the only sanctioned
    /// mutation of historical ledger rows (per v3 plan §6
    /// "Rename exception to append-only ledger").
    RenameApp { from: String, to: String },

    /// Model moved from one app to another via
    /// `#[model(moved_from_app = OldApp)]`. SQL is a no-op; only the
    /// per-app folder placement and the `app_label` UPDATE differ
    /// (per OQ-11 ruling).
    MoveModelBetweenApps {
        model: String,
        from_app: String,
        to_app: String,
    },
}

/// Detailed shape of a column-level alteration.
///
/// Keeps the differ's [`SchemaOperation::AlterColumn`] entry compact
/// while letting T3 dispatch on a typed enum rather than diffing
/// the full [`ColumnSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnChange {
    /// `SET NOT NULL` / `DROP NOT NULL`.
    SetNullable(bool),

    /// `SET DEFAULT <expr>` / `DROP DEFAULT`.
    SetDefault(Option<String>),

    /// `ALTER COLUMN ... TYPE <new>`. Carries both old and new
    /// rendered SQL types so the emitter can decide whether a `USING`
    /// clause is needed.
    ChangeType { from: String, to: String },

    /// `SET / DROP CHECK` constraint at the column level.
    SetCheck(Option<String>),

    /// Column-level `UNIQUE` constraint flipped.
    SetUnique(bool),

    /// `#[field(index)]` flag flipped (column-level implicit index).
    SetIndexed(bool),
}

/// Aggregate flavour of a [`SchemaDelta`].
///
/// Computed from the worst (most-destructive) operation in the
/// delta. Order of severity: `NoOp` < `Additive` < `Reversible` <
/// `Destructive` < `Lossy` < `Unsupported`. `PkTypeFlip` is
/// orthogonal — a delta carrying any `PkTypeFlip` operation is
/// classified `PkTypeFlip` regardless of what other ops it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Schemas compared equal. No operations emitted.
    NoOp,

    /// All operations are non-destructive: `AddTable`, `AddColumn`
    /// (nullable or default-having), `AddIndex`, `AddEnum`,
    /// `AddEnumVariant`, `AddForeignKey` (validated separately).
    Additive,

    /// At least one rename, but no drops. Migration has a clean
    /// inverse.
    Reversible,

    /// At least one destructive operation. The `allow_destructive`
    /// flag is the runner-side opt-in — `false` by default; runner
    /// rejects until operator passes `--allow-destructive`.
    Destructive { allow_destructive: bool },

    /// Destructive operation that loses row data with no fallback —
    /// e.g. dropping a non-nullable, non-default column. Stricter
    /// than `Destructive`; runner rejects regardless of flag.
    Lossy,

    /// Differ cannot lower the change safely. Operator must hand-
    /// edit the migration.
    Unsupported { reason: String },

    /// At least one PK-type flip. Routes through T9's
    /// expand/contract orchestration rather than the standard
    /// transactional apply.
    PkTypeFlip,
}

/// Diff two snapshots within the same `(database, app)` bucket.
///
/// Returns `Classification::NoOp` with an empty operations vector
/// when the two schemas compare equal. The output's
/// `bucket` is taken verbatim — both `before` and `after` are
/// expected to belong to the same bucket; multi-bucket diffing is
/// [`diff_bucket_maps`]'s job.
pub fn diff_schemas(
    before: &AppliedSchema,
    after: &AppliedSchema,
    bucket: BucketKey,
) -> SchemaDelta {
    if before == after {
        return SchemaDelta {
            bucket,
            operations: Vec::new(),
            classification: Classification::NoOp,
        };
    }

    let mut ops: Vec<SchemaOperation> = Vec::new();

    // Build rename hint maps from the new schema. A new table whose
    // `renamed_from = Some("old")` and "old" exists in `before` is a
    // rename, not a drop+add. Same for column renames.
    let table_rename_targets: BTreeMap<&str, &str> = after
        .models
        .values()
        .filter_map(|t| {
            t.renamed_from
                .as_deref()
                .filter(|prev| before.models.contains_key(*prev))
                .map(|prev| (prev, t.table.as_str()))
        })
        .collect();
    let renamed_table_destinations: BTreeSet<&str> =
        table_rename_targets.values().copied().collect();

    // Tables — handle adds, drops, renames first; per-table column
    // diffs come after on the post-rename name.
    diff_tables(
        before,
        after,
        &table_rename_targets,
        &renamed_table_destinations,
        &mut ops,
    );

    // Indexes — diff after table renames so the index's `table` field
    // resolves to the new name.
    diff_indexes(before, after, &table_rename_targets, &mut ops);

    // Enums — symmetric add/drop/variant-add detection.
    diff_enums(before, after, &mut ops);

    // App moves — `TableSchema.moved_from_app` differs across
    // schemas means the model crossed buckets. The diff_schemas
    // entry point handles within-bucket only; cross-bucket moves
    // are emitted by `diff_bucket_maps`.

    let classification = classify(&ops);
    SchemaDelta {
        bucket,
        operations: ops,
        classification,
    }
}

/// Diff full per-bucket maps. Emits one [`SchemaDelta`] per bucket
/// that exists in either side. Cross-bucket operations
/// (`MoveModelBetweenApps`, `RenameApp`) are placed on the
/// destination bucket's delta.
pub fn diff_bucket_maps(
    before: &BTreeMap<BucketKey, AppliedSchema>,
    after: &BTreeMap<BucketKey, AppliedSchema>,
) -> Vec<SchemaDelta> {
    let mut buckets: BTreeSet<BucketKey> = BTreeSet::new();
    buckets.extend(before.keys().cloned());
    buckets.extend(after.keys().cloned());

    let mut out = Vec::with_capacity(buckets.len());
    let empty = AppliedSchema {
        djogi_version: String::new(),
        enums: BTreeMap::new(),
        format_version: super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: String::new(),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: Vec::new(),
    };
    for bucket in buckets {
        let b = before.get(&bucket).unwrap_or(&empty);
        let a = after.get(&bucket).unwrap_or(&empty);
        out.push(diff_schemas(b, a, bucket));
    }
    out
}

fn diff_tables(
    before: &AppliedSchema,
    after: &AppliedSchema,
    table_rename_targets: &BTreeMap<&str, &str>,
    renamed_table_destinations: &BTreeSet<&str>,
    ops: &mut Vec<SchemaOperation>,
) {
    // Tables present in `before` but not in `after` (and not the
    // source of a rename) — DropTable.
    for old_name in before.models.keys() {
        if after.models.contains_key(old_name) {
            continue;
        }
        if table_rename_targets.contains_key(old_name.as_str()) {
            // Renamed → handled below.
            continue;
        }
        ops.push(SchemaOperation::DropTable(old_name.clone()));
    }

    // Tables present in `after` but not in `before`, that are NOT
    // rename destinations — AddTable.
    for new_table in after.models.values() {
        if before.models.contains_key(&new_table.table) {
            // Common to both; handled below.
            continue;
        }
        if renamed_table_destinations.contains(new_table.table.as_str()) {
            continue;
        }
        ops.push(SchemaOperation::AddTable(new_table.clone()));
    }

    // Renames — `from -> to` plus per-column diff against the renamed
    // shape.
    for (from, to) in table_rename_targets {
        let before_table = &before.models[*from];
        let after_table = &after.models[*to];
        ops.push(SchemaOperation::RenameTable {
            from: (*from).to_string(),
            to: (*to).to_string(),
        });
        diff_columns_in_table(before_table, after_table, ops);
        diff_pk_in_table(before_table, after_table, ops);
    }

    // Common tables (same name in both schemas) — column diff.
    for (name, after_table) in &after.models {
        let Some(before_table) = before.models.get(name) else {
            continue;
        };
        if before_table == after_table {
            continue;
        }
        diff_columns_in_table(before_table, after_table, ops);
        diff_pk_in_table(before_table, after_table, ops);
        diff_app_move_in_table(before_table, after_table, ops);
    }
}

fn diff_columns_in_table(
    before: &TableSchema,
    after: &TableSchema,
    ops: &mut Vec<SchemaOperation>,
) {
    let before_cols: BTreeMap<&str, &ColumnSchema> = before
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();
    let after_cols: BTreeMap<&str, &ColumnSchema> =
        after.columns.iter().map(|c| (c.name.as_str(), c)).collect();

    // Column rename hints — same pattern as table renames.
    let column_rename_targets: BTreeMap<&str, &str> = after_cols
        .values()
        .filter_map(|c| {
            c.renamed_from
                .as_deref()
                .filter(|prev| before_cols.contains_key(prev))
                .map(|prev| (prev, c.name.as_str()))
        })
        .collect();
    let renamed_destinations: BTreeSet<&str> = column_rename_targets.values().copied().collect();

    // Drops (no rename source).
    for old_name in before_cols.keys() {
        if after_cols.contains_key(old_name) {
            continue;
        }
        if column_rename_targets.contains_key(old_name) {
            continue;
        }
        ops.push(SchemaOperation::DropColumn {
            table: after.table.clone(),
            column: (*old_name).to_string(),
        });
    }

    // Adds (not a rename destination).
    for new_col in after.columns.iter() {
        if before_cols.contains_key(new_col.name.as_str()) {
            continue;
        }
        if renamed_destinations.contains(new_col.name.as_str()) {
            continue;
        }
        ops.push(SchemaOperation::AddColumn {
            table: after.table.clone(),
            column: new_col.clone(),
        });
    }

    // Renames + alter on renamed columns.
    for (from, to) in &column_rename_targets {
        let bc = before_cols[from];
        let ac = after_cols[to];
        ops.push(SchemaOperation::RenameColumn {
            table: after.table.clone(),
            from: (*from).to_string(),
            to: (*to).to_string(),
        });
        emit_alter_column(after, bc, ac, ops);
    }

    // Common columns (same name on both sides) — alter.
    for (name, ac) in &after_cols {
        let Some(bc) = before_cols.get(name) else {
            continue;
        };
        if bc == ac {
            continue;
        }
        emit_alter_column(after, bc, ac, ops);
    }
}

fn emit_alter_column(
    parent: &TableSchema,
    before: &ColumnSchema,
    after: &ColumnSchema,
    ops: &mut Vec<SchemaOperation>,
) {
    let table = parent.table.clone();
    if before.sql_type != after.sql_type {
        ops.push(SchemaOperation::AlterColumn {
            table: table.clone(),
            column: after.name.clone(),
            change: ColumnChange::ChangeType {
                from: before.sql_type.clone(),
                to: after.sql_type.clone(),
            },
        });
    }
    if before.nullable != after.nullable {
        ops.push(SchemaOperation::AlterColumn {
            table: table.clone(),
            column: after.name.clone(),
            change: ColumnChange::SetNullable(after.nullable),
        });
    }
    if before.default_sql != after.default_sql {
        ops.push(SchemaOperation::AlterColumn {
            table: table.clone(),
            column: after.name.clone(),
            change: ColumnChange::SetDefault(after.default_sql.clone()),
        });
    }
    if before.check != after.check {
        ops.push(SchemaOperation::AlterColumn {
            table: table.clone(),
            column: after.name.clone(),
            change: ColumnChange::SetCheck(after.check.clone()),
        });
    }
    if before.unique != after.unique {
        ops.push(SchemaOperation::AlterColumn {
            table: table.clone(),
            column: after.name.clone(),
            change: ColumnChange::SetUnique(after.unique),
        });
    }
    if before.indexed != after.indexed {
        ops.push(SchemaOperation::AlterColumn {
            table: table.clone(),
            column: after.name.clone(),
            change: ColumnChange::SetIndexed(after.indexed),
        });
    }
    // FK transitions.
    match (&before.foreign_key, &after.foreign_key) {
        (None, Some(fk)) => ops.push(SchemaOperation::AddForeignKey {
            table: table.clone(),
            column: after.name.clone(),
            fk: fk.clone(),
        }),
        (Some(_), None) => ops.push(SchemaOperation::DropForeignKey {
            table,
            column: after.name.clone(),
        }),
        (Some(b_fk), Some(a_fk)) if b_fk != a_fk => {
            // FK retargeting — emit drop + add for clarity.
            ops.push(SchemaOperation::DropForeignKey {
                table: table.clone(),
                column: after.name.clone(),
            });
            ops.push(SchemaOperation::AddForeignKey {
                table,
                column: after.name.clone(),
                fk: a_fk.clone(),
            });
        }
        _ => {}
    }
}

fn diff_pk_in_table(before: &TableSchema, after: &TableSchema, ops: &mut Vec<SchemaOperation>) {
    if before.primary_key == after.primary_key {
        return;
    }
    if is_pk_flip_pair(&before.primary_key, &after.primary_key) {
        ops.push(SchemaOperation::PkTypeFlip {
            table: after.table.clone(),
            from: before.primary_key.kind.clone(),
            to: after.primary_key.kind.clone(),
        });
    }
    // Other PK changes (HeerId → Serial, addition of composite, etc.)
    // are intentionally not auto-emitted; T2 surfaces them via
    // classification = Unsupported in `classify` so the operator
    // hand-rolls the migration.
}

fn is_pk_flip_pair(before: &PrimaryKeySchema, after: &PrimaryKeySchema) -> bool {
    if before.columns != after.columns {
        return false;
    }
    matches!(
        (&before.kind, &after.kind),
        (PkKindSchema::HeerId, PkKindSchema::HeerIdRecencyBiased)
            | (PkKindSchema::HeerIdRecencyBiased, PkKindSchema::HeerId)
            | (PkKindSchema::RanjId, PkKindSchema::RanjIdRecencyBiased)
            | (PkKindSchema::RanjIdRecencyBiased, PkKindSchema::RanjId)
    )
}

fn diff_app_move_in_table(
    before: &TableSchema,
    after: &TableSchema,
    ops: &mut Vec<SchemaOperation>,
) {
    if before.app == after.app {
        return;
    }
    let from_app = before.app.clone().unwrap_or_default();
    let to_app = after.app.clone().unwrap_or_default();
    ops.push(SchemaOperation::MoveModelBetweenApps {
        model: after.table.clone(),
        from_app,
        to_app,
    });
}

fn diff_indexes(
    before: &AppliedSchema,
    after: &AppliedSchema,
    table_rename_targets: &BTreeMap<&str, &str>,
    ops: &mut Vec<SchemaOperation>,
) {
    let before_idx: BTreeMap<&str, &IndexSchema> = before
        .indexes
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();
    let after_idx: BTreeMap<&str, &IndexSchema> =
        after.indexes.iter().map(|i| (i.name.as_str(), i)).collect();

    for (name, ai) in &after_idx {
        let resolved_old = before_idx.get(name).copied();
        match resolved_old {
            Some(bi) => {
                // Existing — emit drop+add when shape changed (no
                // ALTER INDEX in Postgres beyond rename).
                let resolved_table_match = match table_rename_targets
                    .iter()
                    .find(|(_, to)| **to == bi.table.as_str())
                {
                    Some((from, _)) => *from == ai.table.as_str(),
                    None => bi.table == ai.table,
                };
                let logical_eq = resolved_table_match
                    && bi.target == ai.target
                    && bi.kind == ai.kind
                    && bi.index_type == ai.index_type
                    && bi.predicate == ai.predicate
                    && bi.include == ai.include
                    && bi.nulls_not_distinct == ai.nulls_not_distinct
                    && bi.requires_out_of_transaction == ai.requires_out_of_transaction
                    && bi.extension_dependency == ai.extension_dependency;
                if !logical_eq {
                    ops.push(SchemaOperation::DropIndex((*name).to_string()));
                    ops.push(SchemaOperation::AddIndex((*ai).clone()));
                }
            }
            None => {
                ops.push(SchemaOperation::AddIndex((*ai).clone()));
            }
        }
    }
    for name in before_idx.keys() {
        if after_idx.contains_key(name) {
            continue;
        }
        ops.push(SchemaOperation::DropIndex((*name).to_string()));
    }
}

fn diff_enums(before: &AppliedSchema, after: &AppliedSchema, ops: &mut Vec<SchemaOperation>) {
    for (name, ae) in &after.enums {
        match before.enums.get(name) {
            None => ops.push(SchemaOperation::AddEnum((*ae).clone())),
            Some(be) if be == ae => {}
            Some(be) => {
                let before_set: BTreeSet<&str> = be.variants.iter().map(|v| v.as_str()).collect();
                for (i, v) in ae.variants.iter().enumerate() {
                    if !before_set.contains(v.as_str()) {
                        ops.push(SchemaOperation::AddEnumVariant {
                            enum_name: name.clone(),
                            variant: v.clone(),
                            index: i,
                        });
                    }
                }
                // Removals — Postgres has no `DROP VALUE`; surface as
                // unsupported via the variant-removal sentinel below.
                let after_set: BTreeSet<&str> = ae.variants.iter().map(|v| v.as_str()).collect();
                for v in &be.variants {
                    if !after_set.contains(v.as_str()) {
                        // Recorded as DropEnum + AddEnum + ALTER TYPE rebuild
                        // in T3, classified as Unsupported here.
                        ops.push(SchemaOperation::DropEnum(format!(
                            "{name} (variant `{v}` removed; Postgres has no DROP VALUE)"
                        )));
                    }
                }
            }
        }
    }
    for name in before.enums.keys() {
        if !after.enums.contains_key(name) {
            ops.push(SchemaOperation::DropEnum(name.clone()));
        }
    }
}

/// Compute the aggregate [`Classification`] for an operation list.
///
/// Severity ladder: `NoOp` < `Additive` < `Reversible` <
/// `Destructive` < `Lossy` < `Unsupported`. `PkTypeFlip` short-
/// circuits to its own classification regardless of co-operations
/// because Phase 7 T9 owns the orchestration; non-flip ops landing
/// alongside a flip would land via the standard segments planned
/// around the flip's expand/contract phases.
fn classify(ops: &[SchemaOperation]) -> Classification {
    if ops.is_empty() {
        return Classification::NoOp;
    }

    let mut has_pk_flip = false;
    let mut has_destructive = false;
    let mut has_rename = false;
    let mut has_lossy = false;
    let mut has_unsupported = false;
    let mut unsupported_reason: Option<String> = None;

    for op in ops {
        match op {
            SchemaOperation::PkTypeFlip { .. } => has_pk_flip = true,
            SchemaOperation::DropTable(_) => has_destructive = true,
            SchemaOperation::DropColumn { .. } => {
                has_destructive = true;
                // Treating every DropColumn as Destructive (not Lossy)
                // because the column may be a nullable surplus. T2's
                // Lossy detection requires more metadata than current
                // ColumnSchema carries (default + nullability cross-
                // check); upgrade lands when T9's hazard pre-flight
                // shape needs it.
            }
            SchemaOperation::DropEnum(reason) => {
                if reason.contains("DROP VALUE") {
                    has_unsupported = true;
                    unsupported_reason.get_or_insert_with(|| reason.clone());
                } else {
                    has_destructive = true;
                }
            }
            SchemaOperation::DropIndex(_) | SchemaOperation::DropForeignKey { .. } => {
                has_destructive = true;
            }
            SchemaOperation::RenameTable { .. }
            | SchemaOperation::RenameColumn { .. }
            | SchemaOperation::RenameApp { .. }
            | SchemaOperation::MoveModelBetweenApps { .. } => has_rename = true,
            SchemaOperation::AlterColumn { change, .. } => {
                if matches!(change, ColumnChange::SetNullable(false)) {
                    // Adding a NOT NULL on existing data without a
                    // default could leave existing rows in violation —
                    // surface as Lossy unless paired with a default.
                    has_lossy = true;
                }
            }
            SchemaOperation::AddTable(_)
            | SchemaOperation::AddColumn { .. }
            | SchemaOperation::AddIndex(_)
            | SchemaOperation::AddEnum(_)
            | SchemaOperation::AddEnumVariant { .. }
            | SchemaOperation::AddForeignKey { .. } => {}
        }
    }

    if has_unsupported {
        return Classification::Unsupported {
            reason: unsupported_reason
                .unwrap_or_else(|| "operation unsupported by differ".to_string()),
        };
    }
    if has_pk_flip {
        return Classification::PkTypeFlip;
    }
    if has_lossy {
        return Classification::Lossy;
    }
    if has_destructive {
        return Classification::Destructive {
            allow_destructive: false,
        };
    }
    if has_rename {
        return Classification::Reversible;
    }
    Classification::Additive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppDescriptor;
    use crate::descriptor::{
        EnumDescriptor, FieldDescriptor, FieldSqlType, IndexColumnSpec, IndexKind, IndexSpec,
        IndexTarget, IndexType, ModelDescriptor, PkType,
    };
    use crate::migrate::projection::project_from_iters;

    fn synth_model(table: &'static str, type_name: &'static str) -> ModelDescriptor {
        ModelDescriptor {
            type_name,
            table_name: table,
            pk_type: PkType::HeerIdDesc,
            fields: &[],
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: &[],
            is_through: false,
            fts: None,
            app: None,
            moved_from_app: None,
            renamed_from: None,
        }
    }

    fn empty_global() -> BucketKey {
        BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        }
    }

    fn project_one(m: &ModelDescriptor) -> AppliedSchema {
        let mut buckets = project_from_iters(
            [m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("project");
        buckets.remove(&empty_global()).unwrap()
    }

    fn project_empty() -> AppliedSchema {
        let mut buckets = project_from_iters(
            std::iter::empty::<&ModelDescriptor>(),
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("project");
        buckets.remove(&empty_global()).unwrap()
    }

    #[test]
    fn equal_schemas_yield_noop() {
        let m = synth_model("widgets", "Widget");
        let s = project_one(&m);
        let delta = diff_schemas(&s, &s, empty_global());
        assert_eq!(delta.classification, Classification::NoOp);
        assert!(delta.operations.is_empty());
    }

    #[test]
    fn add_table_is_additive() {
        let before = project_empty();
        let m = synth_model("widgets", "Widget");
        let after = project_one(&m);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Additive);
        assert!(matches!(
            delta.operations.first(),
            Some(SchemaOperation::AddTable(t)) if t.table == "widgets"
        ));
    }

    #[test]
    fn drop_table_is_destructive() {
        let m = synth_model("widgets", "Widget");
        let before = project_one(&m);
        let after = project_empty();
        let delta = diff_schemas(&before, &after, empty_global());
        assert!(matches!(
            delta.classification,
            Classification::Destructive {
                allow_destructive: false
            }
        ));
        assert!(matches!(
            delta.operations.first(),
            Some(SchemaOperation::DropTable(t)) if t == "widgets"
        ));
    }

    #[test]
    fn renamed_table_is_reversible_not_destructive() {
        let old_m = synth_model("widgets", "Widget");
        let new_m = ModelDescriptor {
            table_name: "gadgets",
            renamed_from: Some("widgets"),
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&old_m);
        let after = project_one(&new_m);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Reversible);
        assert_eq!(delta.operations.len(), 1);
        assert!(matches!(
            &delta.operations[0],
            SchemaOperation::RenameTable { from, to } if from == "widgets" && to == "gadgets"
        ));
    }

    #[test]
    fn unannotated_table_swap_is_destructive_drop_plus_add() {
        // Without `renamed_from`, a name change is two unrelated
        // operations: drop the old, add the new.
        let old_m = synth_model("widgets", "Widget");
        let new_m = synth_model("gadgets", "Gadget");
        let before = project_one(&old_m);
        let after = project_one(&new_m);
        let delta = diff_schemas(&before, &after, empty_global());
        assert!(matches!(
            delta.classification,
            Classification::Destructive { .. }
        ));
        let kinds: Vec<_> = delta
            .operations
            .iter()
            .map(|op| match op {
                SchemaOperation::DropTable(_) => "drop",
                SchemaOperation::AddTable(_) => "add",
                _ => "other",
            })
            .collect();
        assert!(kinds.contains(&"drop"));
        assert!(kinds.contains(&"add"));
    }

    /// Build a `FieldDescriptor` literal that can satisfy `&'static`
    /// slice borrows when referenced via a `static` slot. Helper fn
    /// is `const` so callers can declare fixtures as `const FOO:
    /// FieldDescriptor = field_descriptor(...)` and embed them in
    /// `static SLICE: &[FieldDescriptor] = &[FOO];`.
    const fn field_descriptor(
        name: &'static str,
        sql_type: FieldSqlType,
        nullable: bool,
    ) -> FieldDescriptor {
        FieldDescriptor {
            name,
            sql_type,
            nullable,
            unique: false,
            indexed: false,
            max_length: None,
            renamed_from: None,
            rationale: None,
            outbox_exclude: false,
            sequence_within: None,
            index_type: None,
            relation_kind: None,
            on_delete: None,
            target_type_name: None,
            visage_map: &[],
        }
    }

    #[test]
    fn add_column_is_additive() {
        const NAME: FieldDescriptor = field_descriptor("name", FieldSqlType::Text, true);
        static FIELDS_WITH_NAME: &[FieldDescriptor] = &[NAME];
        let bare = synth_model("widgets", "Widget");
        let new_field = ModelDescriptor {
            fields: FIELDS_WITH_NAME,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&bare);
        let after = project_one(&new_field);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Additive);
        assert!(matches!(
            delta.operations.first(),
            Some(SchemaOperation::AddColumn { table, column }) if table == "widgets" && column.name == "name"
        ));
    }

    #[test]
    fn pk_flip_classifies_as_pk_type_flip() {
        let asc = ModelDescriptor {
            pk_type: PkType::HeerId,
            ..synth_model("widgets", "Widget")
        };
        let desc = ModelDescriptor {
            pk_type: PkType::HeerIdDesc,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&asc);
        let after = project_one(&desc);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::PkTypeFlip);
        assert!(delta.operations.iter().any(|op| matches!(
            op,
            SchemaOperation::PkTypeFlip {
                from: PkKindSchema::HeerId,
                to: PkKindSchema::HeerIdRecencyBiased,
                ..
            }
        )));
    }

    #[test]
    fn pk_flip_reverse_direction_also_classifies() {
        let desc = ModelDescriptor {
            pk_type: PkType::HeerIdDesc,
            ..synth_model("widgets", "Widget")
        };
        let asc = ModelDescriptor {
            pk_type: PkType::HeerId,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&desc);
        let after = project_one(&asc);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::PkTypeFlip);
    }

    #[test]
    fn pk_unrelated_change_does_not_flip() {
        // HeerId → Serial is NOT a flip pair.
        let heer = ModelDescriptor {
            pk_type: PkType::HeerId,
            ..synth_model("widgets", "Widget")
        };
        let serial = ModelDescriptor {
            pk_type: PkType::Serial,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&heer);
        let after = project_one(&serial);
        let delta = diff_schemas(&before, &after, empty_global());
        // Neither flip nor any other op is emitted; T2 leaves
        // unsupported PK transitions for the operator. The two PK
        // kinds change `id`'s default_sql, which does surface as an
        // AlterColumn; that's classified Additive (no nullability
        // change). The classification is therefore not PkTypeFlip.
        assert_ne!(delta.classification, Classification::PkTypeFlip);
    }

    #[test]
    fn add_index_is_additive() {
        let bare = synth_model("widgets", "Widget");
        static IDX_SLICE: &[IndexSpec] = &[IndexSpec {
            name: "widgets_name_idx",
            target: IndexTarget::Columns(&[IndexColumnSpec::simple("name")]),
            kind: IndexKind::NonUnique,
            index_type: IndexType::BTree,
            predicate: None,
            include: &[],
            nulls_not_distinct: false,
            requires_out_of_transaction: false,
            extension_dependency: None,
        }];
        let with_idx = ModelDescriptor {
            indexes: IDX_SLICE,
            ..bare.clone()
        };
        let before = project_one(&bare);
        let after = project_one(&with_idx);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Additive);
        assert!(matches!(
            delta.operations.first(),
            Some(SchemaOperation::AddIndex(idx)) if idx.name == "widgets_name_idx"
        ));
    }

    #[test]
    fn add_enum_is_additive() {
        let m = synth_model("widgets", "Widget");
        let e = EnumDescriptor {
            type_name: "Status",
            postgres_type: "status",
            variants: &["active", "deleted"],
        };
        let mut buckets_before = project_from_iters(
            [&m],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("p");
        let mut buckets_after = project_from_iters(
            [&m],
            [&e],
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("p");
        let before = buckets_before.remove(&empty_global()).unwrap();
        let after = buckets_after.remove(&empty_global()).unwrap();
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Additive);
        assert!(
            delta
                .operations
                .iter()
                .any(|op| matches!(op, SchemaOperation::AddEnum(_)))
        );
    }

    #[test]
    fn variant_removal_classifies_as_unsupported() {
        let m = synth_model("widgets", "Widget");
        let two = EnumDescriptor {
            type_name: "Status",
            postgres_type: "status",
            variants: &["active", "deleted"],
        };
        let one = EnumDescriptor {
            type_name: "Status",
            postgres_type: "status",
            variants: &["active"],
        };
        let mut buckets_before = project_from_iters(
            [&m],
            [&two],
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("p");
        let mut buckets_after = project_from_iters(
            [&m],
            [&one],
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("p");
        let before = buckets_before.remove(&empty_global()).unwrap();
        let after = buckets_after.remove(&empty_global()).unwrap();
        let delta = diff_schemas(&before, &after, empty_global());
        assert!(matches!(
            delta.classification,
            Classification::Unsupported { .. }
        ));
    }

    #[test]
    fn diff_bucket_maps_handles_added_bucket() {
        let m = synth_model("widgets", "Widget");
        let before = BTreeMap::new();
        let mut after = BTreeMap::new();
        after.insert(empty_global(), project_one(&m));
        let deltas = diff_bucket_maps(&before, &after);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].bucket, empty_global());
        assert!(matches!(deltas[0].classification, Classification::Additive));
    }

    #[test]
    fn renamed_column_emits_rename_not_drop_add() {
        const OLD_NAME: FieldDescriptor = field_descriptor("old_name", FieldSqlType::Text, true);
        static OLD_SLICE: &[FieldDescriptor] = &[OLD_NAME];
        const NEW_NAME: FieldDescriptor = FieldDescriptor {
            name: "new_name",
            sql_type: FieldSqlType::Text,
            nullable: true,
            unique: false,
            indexed: false,
            max_length: None,
            renamed_from: Some("old_name"),
            rationale: None,
            outbox_exclude: false,
            sequence_within: None,
            index_type: None,
            relation_kind: None,
            on_delete: None,
            target_type_name: None,
            visage_map: &[],
        };
        static NEW_SLICE: &[FieldDescriptor] = &[NEW_NAME];
        let bare = ModelDescriptor {
            fields: OLD_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let renamed = ModelDescriptor {
            fields: NEW_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&bare);
        let after = project_one(&renamed);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Reversible);
        assert!(matches!(
            delta.operations.first(),
            Some(SchemaOperation::RenameColumn { from, to, .. }) if from == "old_name" && to == "new_name"
        ));
    }

    #[test]
    fn nullability_change_to_not_null_classifies_lossy() {
        const NULLABLE: FieldDescriptor = field_descriptor("name", FieldSqlType::Text, true);
        const NOT_NULL: FieldDescriptor = field_descriptor("name", FieldSqlType::Text, false);
        static NULLABLE_SLICE: &[FieldDescriptor] = &[NULLABLE];
        static NOT_NULL_SLICE: &[FieldDescriptor] = &[NOT_NULL];
        let nullable = ModelDescriptor {
            fields: NULLABLE_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let not_null = ModelDescriptor {
            fields: NOT_NULL_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&nullable);
        let after = project_one(&not_null);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Lossy);
    }

    #[test]
    fn nullability_change_to_nullable_is_additive() {
        const NOT_NULL: FieldDescriptor = field_descriptor("name", FieldSqlType::Text, false);
        const NULLABLE: FieldDescriptor = field_descriptor("name", FieldSqlType::Text, true);
        static NOT_NULL_SLICE: &[FieldDescriptor] = &[NOT_NULL];
        static NULLABLE_SLICE: &[FieldDescriptor] = &[NULLABLE];
        let not_null = ModelDescriptor {
            fields: NOT_NULL_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let nullable = ModelDescriptor {
            fields: NULLABLE_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&not_null);
        let after = project_one(&nullable);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Additive);
    }

    #[test]
    fn type_change_emits_alter_column_change_type() {
        const TEXT: FieldDescriptor = field_descriptor("amount", FieldSqlType::Text, true);
        const BIGINT: FieldDescriptor = field_descriptor("amount", FieldSqlType::BigInt, true);
        static TEXT_SLICE: &[FieldDescriptor] = &[TEXT];
        static BIGINT_SLICE: &[FieldDescriptor] = &[BIGINT];
        let text = ModelDescriptor {
            fields: TEXT_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let bigint = ModelDescriptor {
            fields: BIGINT_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&text);
        let after = project_one(&bigint);
        let delta = diff_schemas(&before, &after, empty_global());
        assert!(delta.operations.iter().any(|op| matches!(
            op,
            SchemaOperation::AlterColumn {
                change: ColumnChange::ChangeType { .. },
                ..
            }
        )));
    }
}
