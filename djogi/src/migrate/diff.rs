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
//! - `Destructive` — at least one operation removes data
//!   structurally (`DropTable`, `DropColumn`, `DropEnum`,
//!   `DropIndex`, `DropForeignKey`). The runner gates these behind
//!   `--allow-destructive`; the gate is runner-side state, not
//!   differ output, so the variant carries no payload.
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
    TableSchema,
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
    /// `REFERENCES` constraint is removed. The full
    /// [`ForeignKeySchema`] (including the cascade discipline that
    /// existed on the old side) is carried so T3 can emit a
    /// reversible rollback that restores the original `FOREIGN KEY
    /// ... REFERENCES ... ON DELETE ...` clause without operator
    /// hand-edit. Codex T3 review B-3 fixed an earlier bug where the
    /// drop carried only `(table, column)` and the rollback was a
    /// SQL comment.
    DropForeignKey {
        table: String,
        column: String,
        fk: ForeignKeySchema,
    },

    /// Index added.
    AddIndex(IndexSchema),

    /// Index dropped — carries the full [`IndexSchema`] so T3's
    /// segment planner can preserve the old index's
    /// `requires_out_of_transaction`, `extension_dependency`, and
    /// uniqueness shape when laddering the drop into the right
    /// segment kind. Dropping just the name (the previous shape)
    /// forced T3 to re-derive metadata it shouldn't need to recover.
    DropIndex(IndexSchema),

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
        /// Anchor for the `BEFORE` / `AFTER` placement clause. `None`
        /// means "append at the tail" (no positional clause). The
        /// differ chooses, in priority order:
        ///
        /// 1. `Some(EnumVariantAnchor { kind: Before, variant: post })`
        ///    when there is a post-anchor variant in the new list
        ///    that already existed in the old list. This places the
        ///    new variant immediately before that anchor.
        /// 2. `Some(EnumVariantAnchor { kind: After, variant: pre })`
        ///    when there is no usable post-anchor but a pre-anchor
        ///    exists in both old and new lists. This is the case for
        ///    a tail-append onto an enum that already has variants
        ///    from the old list — the new variant lands `AFTER` the
        ///    last existing one. (`ALTER TYPE ... ADD VALUE 'x' AFTER
        ///    'y'` is deterministic Postgres DDL, so anchoring beats
        ///    bare append even though both produce the same physical
        ///    ordering.)
        /// 3. `None` only when no anchor in the old list is reachable
        ///    in either direction — e.g. every old variant has been
        ///    concurrently dropped (which the differ would already
        ///    have rejected as `Unsupported` upstream). In practice
        ///    [`pick_enum_variant_anchor`] returns `None` exclusively
        ///    on this degenerate input; tail-appends with prior real
        ///    variants always land in case (2).
        ///
        /// Codex T3 review B-2 fixed an earlier bug where the
        /// emitter unconditionally appended (no `BEFORE`/`AFTER`
        /// clause) regardless of where the differ placed the variant.
        /// Carrying an anchor variant name (rather than a positional
        /// integer) makes the emission self-contained — the emitter
        /// no longer needs the full new-variant list to resolve a
        /// position. Codex T3 round-2 review N-1 tightened the
        /// description here to match the helper's actual behaviour
        /// for tail-appends onto a non-empty enum.
        anchor: Option<EnumVariantAnchor>,
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

    /// Catch-all for changes the differ recognised but cannot lower
    /// safely — non-flip PK transitions (e.g. `HeerId → Serial`),
    /// enum variant removals, partition method changes. Carries an
    /// operator-actionable reason. Drives
    /// [`Classification::Unsupported`].
    Unsupported { reason: String },
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

/// Anchor variant for an [`SchemaOperation::AddEnumVariant`] insertion.
///
/// Postgres `ALTER TYPE ... ADD VALUE 'new' [BEFORE|AFTER 'anchor']`
/// requires a real existing variant as the anchor. The differ
/// picks the anchor by walking the new variant list around the
/// inserted variant and finding the nearest neighbour that already
/// exists in the old list — see the doc on
/// [`SchemaOperation::AddEnumVariant::anchor`] for the exact priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumVariantAnchor {
    /// The existing variant the new value should be placed relative
    /// to. Must already exist in the enum at the time `ALTER TYPE`
    /// runs.
    pub variant: String,
    /// Whether to place the new variant before or after the anchor.
    pub kind: EnumVariantAnchorKind,
}

/// Direction of an [`EnumVariantAnchor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumVariantAnchorKind {
    /// `ALTER TYPE ... ADD VALUE 'new' BEFORE 'anchor'` — anchor is
    /// the variant that should sort immediately AFTER the new one.
    Before,
    /// `ALTER TYPE ... ADD VALUE 'new' AFTER 'anchor'` — anchor is
    /// the variant that should sort immediately BEFORE the new one.
    After,
}

/// Aggregate flavour of a [`SchemaDelta`].
///
/// Computed from the operations in the delta. Severity ladder:
/// `NoOp` < `Additive` < `Reversible` < `Destructive` < `Lossy` <
/// `Unsupported`. `PkTypeFlip` is orthogonal: any delta carrying a
/// `PkTypeFlip` operation classifies as `PkTypeFlip`, but the
/// `co_destructive` / `co_lossy` flags surface co-existing severity
/// so T9's orchestration knows whether the delta also drops a
/// column / index / FK or tightens a nullability — letting the
/// `--allow-destructive` gate fire even when the headline
/// classification is the flip.
///
/// `Destructive` no longer carries the runner-side
/// `allow_destructive` flag — that's a `djogi migrations apply`
/// argument, not a differ property. The runner reads its own opt-in
/// flag and gates `Destructive` / `Lossy` accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Schemas compared equal. No operations emitted.
    NoOp,

    /// All operations are non-destructive: `AddTable`, `AddColumn`
    /// (nullable or default-having), `AddIndex`, `AddEnum`,
    /// `AddEnumVariant`, `AddForeignKey`.
    Additive,

    /// At least one rename, but no drops. Migration has a clean
    /// inverse.
    Reversible,

    /// At least one destructive operation (`DropTable`,
    /// `DropColumn`, `DropIndex`, `DropForeignKey`, `DropEnum`).
    /// Runner rejects until `--allow-destructive` is passed.
    Destructive,

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
    ///
    /// `co_destructive` / `co_lossy` surface co-existing severity so
    /// T9's gate logic can apply `--allow-destructive` semantics
    /// even when the headline classification is the flip. Without
    /// these flags a delta containing both `PkTypeFlip` and
    /// `DropColumn` would silently bypass the destructive gate.
    PkTypeFlip {
        co_destructive: bool,
        co_lossy: bool,
    },
}

/// Diff two snapshots within the same `(database, app)` bucket.
///
/// Returns `Classification::NoOp` with an empty operations vector
/// when the two schemas compare equal. The output's `bucket` is
/// taken verbatim — both `before` and `after` are expected to belong
/// to the same bucket; multi-bucket diffing is
/// [`diff_bucket_maps`]'s job.
///
/// `pub(crate)` because external consumers should always go through
/// [`diff_bucket_maps`] (which handles cross-bucket moves
/// correctly). `diff_schemas` is exposed within the crate for tests
/// and for the per-bucket worker in `diff_bucket_maps`.
pub(crate) fn diff_schemas(
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
    // Stage 1 — pre-scan for cross-bucket moves driven by
    // `TableSchema.moved_from_app`. A model with `moved_from_app =
    // Some("billing")` in the after-schema, whose table name was
    // present in the before-schema's `(database, "billing")` bucket,
    // is a single logical move. Without this pre-scan, the per-bucket
    // diff would emit a spurious `DropTable` on the source bucket
    // and `AddTable` on the destination bucket — losing the
    // semantics of the move and forcing a destructive classification
    // when the change is structurally additive.
    let mut moves: Vec<(BucketKey, BucketKey, String)> = Vec::new();
    let mut suppressed_drops: BTreeSet<(BucketKey, String)> = BTreeSet::new();
    let mut suppressed_adds: BTreeSet<(BucketKey, String)> = BTreeSet::new();
    for (dest_bucket, dest_schema) in after {
        for (table_name, table) in &dest_schema.models {
            let Some(from_app) = table.moved_from_app.as_deref() else {
                continue;
            };
            // Source bucket = same database, prior app label.
            let src_bucket = BucketKey {
                database: dest_bucket.database.clone(),
                app: from_app.to_string(),
            };
            let Some(src_schema) = before.get(&src_bucket) else {
                // Source bucket isn't in `before`. The
                // `moved_from_app` annotation is unrooted; treat as
                // ordinary AddTable on the destination side.
                continue;
            };
            if !src_schema.models.contains_key(table_name) {
                // Source bucket exists but the table isn't there.
                // Same outcome — treat as ordinary AddTable.
                continue;
            }
            moves.push((src_bucket.clone(), dest_bucket.clone(), table_name.clone()));
            suppressed_drops.insert((src_bucket, table_name.clone()));
            suppressed_adds.insert((dest_bucket.clone(), table_name.clone()));
        }
    }

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
        let mut delta = diff_schemas(b, a, bucket.clone());

        // Suppress DropTable on the source bucket and AddTable on
        // the destination bucket for any logical cross-bucket move.
        delta.operations.retain(|op| match op {
            SchemaOperation::DropTable(name) => {
                !suppressed_drops.contains(&(bucket.clone(), name.clone()))
            }
            SchemaOperation::AddTable(t) => {
                !suppressed_adds.contains(&(bucket.clone(), t.table.clone()))
            }
            _ => true,
        });

        // Emit `MoveModelBetweenApps` on the DESTINATION bucket so the
        // operation lands once and on the side T6's compose anchors
        // its `git mv` against.
        for (src_bucket, dest_bucket, model) in &moves {
            if dest_bucket == &bucket {
                delta
                    .operations
                    .push(SchemaOperation::MoveModelBetweenApps {
                        model: model.clone(),
                        from_app: src_bucket.app.clone(),
                        to_app: dest_bucket.app.clone(),
                    });
            }
        }

        // Re-classify after the suppression + emission step.
        delta.classification = classify(&delta.operations);
        out.push(delta);
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
        let column_renames = diff_columns_in_table(before_table, after_table, ops);
        let column_renames_ref: BTreeMap<&str, &str> = column_renames
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        diff_pk_in_table(before_table, after_table, &column_renames_ref, ops);
    }

    // Common tables (same name in both schemas) — column diff.
    for (name, after_table) in &after.models {
        let Some(before_table) = before.models.get(name) else {
            continue;
        };
        if before_table == after_table {
            continue;
        }
        let column_renames = diff_columns_in_table(before_table, after_table, ops);
        let column_renames_ref: BTreeMap<&str, &str> = column_renames
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        diff_pk_in_table(before_table, after_table, &column_renames_ref, ops);
        diff_app_move_in_table(before_table, after_table, ops);
    }
}

fn diff_columns_in_table(
    before: &TableSchema,
    after: &TableSchema,
    ops: &mut Vec<SchemaOperation>,
) -> BTreeMap<String, String> {
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

    // Return the rename map (owned so callers don't need to thread
    // borrows through their own scope) so the PK differ can normalise
    // column names before flip-pair detection — addresses Codex
    // review's "PK column rename + flip" edge case.
    column_rename_targets
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
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
        (Some(b_fk), None) => ops.push(SchemaOperation::DropForeignKey {
            table,
            column: after.name.clone(),
            fk: b_fk.clone(),
        }),
        (Some(b_fk), Some(a_fk)) if b_fk != a_fk => {
            // FK retargeting — emit drop + add for clarity. The drop
            // carries the OLD FK so its rollback can restore the
            // pre-retarget shape; the add carries the NEW FK.
            ops.push(SchemaOperation::DropForeignKey {
                table: table.clone(),
                column: after.name.clone(),
                fk: b_fk.clone(),
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

/// Diff PK shape between `before` and `after`. Recognised flip
/// pairs emit `PkTypeFlip`; every other non-equal PK transition
/// emits `Unsupported` with a specific reason so `classify()` can
/// surface it cleanly. Non-flip transitions handled here include:
///
/// - kind changes outside the flip pairs (e.g. `HeerId → Serial`)
/// - column-set changes (composite ↔ single, or composite reshape
///   that survives column-rename normalisation)
/// - custom PK shape changes
///
/// **PK column rename + supported flip** is recognised as a flip:
/// the `column_renames` map is applied to `before.primary_key.columns`
/// before comparing against `after.primary_key.columns`, so a single
/// `RenameColumn` op + a kind flip together still produce
/// `SchemaOperation::PkTypeFlip` rather than `Unsupported`. The
/// `RenameColumn` was already emitted by `diff_columns_in_table`
/// before this fn runs.
fn diff_pk_in_table(
    before: &TableSchema,
    after: &TableSchema,
    column_renames: &BTreeMap<&str, &str>,
    ops: &mut Vec<SchemaOperation>,
) {
    if before.primary_key == after.primary_key {
        return;
    }
    // Normalise PK columns through the column-rename map so a PK
    // column renamed from `old_id` to `id` doesn't look like a
    // composite-shape change.
    let normalised_before: Vec<&str> = before
        .primary_key
        .columns
        .iter()
        .map(|c| {
            column_renames
                .get(c.as_str())
                .copied()
                .unwrap_or(c.as_str())
        })
        .collect();
    let normalised_after: Vec<&str> = after
        .primary_key
        .columns
        .iter()
        .map(|c| c.as_str())
        .collect();

    let columns_match = normalised_before == normalised_after;
    if columns_match && is_pk_kind_flip(&before.primary_key.kind, &after.primary_key.kind) {
        ops.push(SchemaOperation::PkTypeFlip {
            table: after.table.clone(),
            from: before.primary_key.kind.clone(),
            to: after.primary_key.kind.clone(),
        });
        return;
    }
    // Anything else in the PK changed — surface as Unsupported so
    // the operator hand-rolls a migration. T9's expand/contract
    // playbook only covers the asc↔desc flips today; other PK
    // transitions don't have a generated playbook.
    ops.push(SchemaOperation::Unsupported {
        reason: format!(
            "table `{}`: primary key change is not auto-supported \
             ({:?} → {:?}). Recognised auto-flips: HeerId ↔ \
             HeerIdRecencyBiased, RanjId ↔ RanjIdRecencyBiased \
             with identical column lists. Hand-write this migration.",
            after.table, before.primary_key, after.primary_key
        ),
    });
}

fn is_pk_kind_flip(before: &PkKindSchema, after: &PkKindSchema) -> bool {
    matches!(
        (before, after),
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
                // Resolve the table name through the rename map to
                // detect "logical-equivalent under a table rename"
                // — the index's `table` field changed only because
                // the table itself was renamed, not because the
                // index was retargeted. The map is keyed
                // `old → new`, so look up `bi.table` (the OLD table
                // name) and compare its `new` value against `ai.table`.
                // The earlier search direction (find the entry whose
                // `to` matches `bi.table`) was backwards and could
                // spuriously force drop+add on pure renames.
                let resolved_table_match = match table_rename_targets.get(bi.table.as_str()) {
                    Some(new_name) => *new_name == ai.table.as_str(),
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
                    // Drop + Add carries the full IndexSchema for both
                    // sides so T3's segment planner knows whether the
                    // dropped index was non-transactional, unique,
                    // constraint-backed, or carried an extension
                    // dependency — without that metadata the planner
                    // would have to re-derive it or assume defaults.
                    ops.push(SchemaOperation::DropIndex((*bi).clone()));
                    ops.push(SchemaOperation::AddIndex((*ai).clone()));
                }
            }
            None => {
                ops.push(SchemaOperation::AddIndex((*ai).clone()));
            }
        }
    }
    for (name, bi) in &before_idx {
        if after_idx.contains_key(name) {
            continue;
        }
        ops.push(SchemaOperation::DropIndex((*bi).clone()));
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
                        let anchor = pick_enum_variant_anchor(&ae.variants, i, &before_set);
                        ops.push(SchemaOperation::AddEnumVariant {
                            enum_name: name.clone(),
                            variant: v.clone(),
                            anchor,
                        });
                    }
                }
                // Removals — Postgres has no `DROP VALUE`. Surface
                // via the typed `Unsupported` variant so `classify`
                // routes cleanly to `Classification::Unsupported`
                // without string-matching on the `DropEnum` reason
                // field.
                let after_set: BTreeSet<&str> = ae.variants.iter().map(|v| v.as_str()).collect();
                for v in &be.variants {
                    if !after_set.contains(v.as_str()) {
                        ops.push(SchemaOperation::Unsupported {
                            reason: format!(
                                "enum `{name}`: variant `{v}` removed. Postgres has \
                                 no `DROP VALUE`; rebuild the type via a hand-written \
                                 migration (drop dependent columns, drop type, \
                                 recreate type without the variant, add columns \
                                 back)."
                            ),
                        });
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

/// Pick the BEFORE/AFTER anchor for a newly-inserted enum variant.
///
/// Walks `new_variants` outward from `pos` and returns the first
/// neighbour that already exists in `before_set` (i.e. lives in the
/// old enum). The post-anchor (next index) wins over the pre-anchor
/// (previous index) when both exist — this keeps the convention
/// consistent: "place before the first existing post-neighbour" so
/// chained inserts of multiple new variants in one delta produce a
/// stable, readable migration where each insert anchors against an
/// already-existing variant rather than another freshly-added one.
///
/// Returns `None` when no anchor in `before_set` exists in either
/// direction — that case is "first variants of a brand new enum",
/// which the caller already routes through [`SchemaOperation::AddEnum`]
/// rather than through `AddEnumVariant`, so this fallback is only
/// reached when every variant in the old list has been removed in
/// the new list (which would itself trigger `Unsupported` first).
fn pick_enum_variant_anchor(
    new_variants: &[String],
    pos: usize,
    before_set: &BTreeSet<&str>,
) -> Option<EnumVariantAnchor> {
    // Look forward first — find the nearest already-existing variant
    // that comes after `pos` in the new list.
    for v in new_variants.iter().skip(pos + 1) {
        if before_set.contains(v.as_str()) {
            return Some(EnumVariantAnchor {
                variant: v.clone(),
                kind: EnumVariantAnchorKind::Before,
            });
        }
    }
    // Then backward — find the nearest already-existing predecessor.
    for v in new_variants[..pos].iter().rev() {
        if before_set.contains(v.as_str()) {
            return Some(EnumVariantAnchor {
                variant: v.clone(),
                kind: EnumVariantAnchorKind::After,
            });
        }
    }
    None
}

/// Compute the aggregate [`Classification`] for an operation list.
///
/// Severity ladder: `NoOp` < `Additive` < `Reversible` <
/// `Destructive` < `Lossy` < `Unsupported`. `PkTypeFlip` is
/// orthogonal: when present, it wins as the headline classification,
/// but it carries `co_destructive` / `co_lossy` flags so co-existing
/// destructive or lossy ops still surface to the runner gate.
/// `Unsupported` always wins, even over `PkTypeFlip`, because an
/// unsupported transition has no executable plan at all.
///
/// `AddColumn` is classified per the column's actual shape:
///
/// - nullable, OR has a default → Additive
/// - non-nullable + no default → Lossy (existing rows would violate
///   the constraint immediately on apply)
fn classify(ops: &[SchemaOperation]) -> Classification {
    if ops.is_empty() {
        return Classification::NoOp;
    }

    let mut has_pk_flip = false;
    let mut has_destructive = false;
    let mut has_rename = false;
    let mut has_lossy = false;
    let mut unsupported_reason: Option<String> = None;

    for op in ops {
        match op {
            SchemaOperation::PkTypeFlip { .. } => has_pk_flip = true,
            SchemaOperation::DropTable(_) => has_destructive = true,
            SchemaOperation::DropColumn { .. } => {
                // A DropColumn is at minimum Destructive. We don't
                // currently have the dropped column's full shape
                // here to decide Lossy vs Destructive (the variant
                // carries only `column: String`). If T9's hazard
                // pre-flight needs that distinction, the variant
                // grows then.
                has_destructive = true;
            }
            SchemaOperation::DropEnum(_)
            | SchemaOperation::DropIndex(_)
            | SchemaOperation::DropForeignKey { .. } => {
                has_destructive = true;
            }
            SchemaOperation::Unsupported { reason } => {
                unsupported_reason.get_or_insert_with(|| reason.clone());
            }
            SchemaOperation::RenameTable { .. }
            | SchemaOperation::RenameColumn { .. }
            | SchemaOperation::RenameApp { .. }
            | SchemaOperation::MoveModelBetweenApps { .. } => has_rename = true,
            SchemaOperation::AlterColumn { change, .. } => {
                if matches!(change, ColumnChange::SetNullable(false)) {
                    has_lossy = true;
                }
            }
            SchemaOperation::AddColumn { column, .. } => {
                if !column.nullable && column.default_sql.is_none() {
                    // NOT NULL added on a populated table without a
                    // default → existing rows would immediately
                    // violate the constraint. Lossy classification
                    // forces the operator to either supply a default
                    // or split the migration into add-nullable +
                    // backfill + tighten-not-null.
                    has_lossy = true;
                }
            }
            SchemaOperation::AddTable(_)
            | SchemaOperation::AddIndex(_)
            | SchemaOperation::AddEnum(_)
            | SchemaOperation::AddEnumVariant { .. }
            | SchemaOperation::AddForeignKey { .. } => {}
        }
    }

    // Unsupported wins over everything: there's no apply plan.
    if let Some(reason) = unsupported_reason {
        return Classification::Unsupported { reason };
    }
    // PkTypeFlip is orthogonal — surface co-flags so the gate logic
    // can still apply destructive / lossy semantics.
    if has_pk_flip {
        return Classification::PkTypeFlip {
            co_destructive: has_destructive,
            co_lossy: has_lossy,
        };
    }
    if has_lossy {
        return Classification::Lossy;
    }
    if has_destructive {
        return Classification::Destructive;
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
    use crate::migrate::schema::{IndexTypeSchema, PrimaryKeySchema, SNAPSHOT_FORMAT_VERSION};

    fn synth_app(label: &'static str, database: &'static str) -> AppDescriptor {
        AppDescriptor {
            label,
            database,
            renamed_from: None,
            tombstone: false,
        }
    }

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
        assert_eq!(delta.classification, Classification::Destructive);
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
        assert_eq!(delta.classification, Classification::Destructive);
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
        assert!(matches!(
            delta.classification,
            Classification::PkTypeFlip {
                co_destructive: false,
                co_lossy: false
            }
        ));
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
        assert!(matches!(
            delta.classification,
            Classification::PkTypeFlip { .. }
        ));
    }

    #[test]
    fn pk_unrelated_change_classifies_as_unsupported() {
        // HeerId → Serial is NOT a flip pair. Per the v3 contract,
        // the differ surfaces this as Unsupported so the operator
        // hand-rolls a migration rather than letting it silently
        // degrade to an AlterColumn-only delta.
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
        assert!(matches!(
            delta.classification,
            Classification::Unsupported { .. }
        ));
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

    // ── T2 fixup tests (added 2026-04-25) ───────────────────────────

    #[test]
    fn add_not_null_column_without_default_classifies_lossy() {
        // Codex T2 review B-1: AddColumn must inspect the column's
        // shape. NOT NULL without a default = Lossy because existing
        // rows would immediately violate the constraint on apply.
        const NOT_NULL: FieldDescriptor = field_descriptor("required", FieldSqlType::Text, false);
        static FIELDS: &[FieldDescriptor] = &[NOT_NULL];
        let bare = synth_model("widgets", "Widget");
        let with_required = ModelDescriptor {
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&bare);
        let after = project_one(&with_required);
        let delta = diff_schemas(&before, &after, empty_global());
        assert_eq!(delta.classification, Classification::Lossy);
    }

    #[test]
    fn pk_flip_with_concurrent_drop_surfaces_co_destructive_flag() {
        // Codex T2 review B-1: PkTypeFlip must NOT mask co-existing
        // destructive ops. A flip + DropTable in the same delta
        // classifies as PkTypeFlip { co_destructive: true } so the
        // runner's --allow-destructive gate still fires.
        let asc = ModelDescriptor {
            pk_type: PkType::HeerId,
            ..synth_model("widgets", "Widget")
        };
        let asc_other = ModelDescriptor {
            pk_type: PkType::HeerId,
            ..synth_model("decommissioned", "Decommissioned")
        };
        let mut before_buckets = project_from_iters(
            [&asc, &asc_other],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("project");
        let desc = ModelDescriptor {
            pk_type: PkType::HeerIdDesc,
            ..synth_model("widgets", "Widget")
        };
        // Note: `decommissioned` removed from after, AND `widgets`
        // pk flipped HeerId → HeerIdDesc — combined drop + PK flip
        // in the same delta.
        let mut after_buckets = project_from_iters(
            [&desc],
            std::iter::empty::<&EnumDescriptor>(),
            std::iter::empty::<&AppDescriptor>(),
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("project");
        let before = before_buckets.remove(&empty_global()).unwrap();
        let after = after_buckets.remove(&empty_global()).unwrap();
        let delta = diff_schemas(&before, &after, empty_global());
        match delta.classification {
            Classification::PkTypeFlip {
                co_destructive,
                co_lossy,
            } => {
                assert!(
                    co_destructive,
                    "DropTable alongside PkTypeFlip must set co_destructive"
                );
                assert!(!co_lossy);
            }
            other => panic!("expected PkTypeFlip with co_destructive, got {other:?}"),
        }
    }

    #[test]
    fn drop_index_carries_full_metadata() {
        // Codex T2 review B-4: DropIndex must carry IndexSchema, not
        // just the name, so T3's segment planner knows the dropped
        // index's `requires_out_of_transaction`, kind, etc.
        static IDX_SLICE: &[IndexSpec] = &[IndexSpec {
            name: "widgets_name_idx",
            target: IndexTarget::Columns(&[IndexColumnSpec::simple("name")]),
            kind: IndexKind::NonUnique,
            index_type: IndexType::Gist,
            predicate: None,
            include: &[],
            nulls_not_distinct: false,
            requires_out_of_transaction: true,
            extension_dependency: Some("postgis"),
        }];
        let with_idx = ModelDescriptor {
            indexes: IDX_SLICE,
            ..synth_model("widgets", "Widget")
        };
        let bare = synth_model("widgets", "Widget");
        let before = project_one(&with_idx);
        let after = project_one(&bare);
        let delta = diff_schemas(&before, &after, empty_global());
        let drop = delta
            .operations
            .iter()
            .find_map(|op| match op {
                SchemaOperation::DropIndex(idx) => Some(idx),
                _ => None,
            })
            .expect("DropIndex emitted");
        assert_eq!(drop.name, "widgets_name_idx");
        assert!(drop.requires_out_of_transaction);
        assert_eq!(drop.extension_dependency.as_deref(), Some("postgis"));
        assert_eq!(drop.index_type, IndexTypeSchema::Gist);
    }

    #[test]
    fn cross_bucket_move_via_moved_from_app_emits_move_not_drop_add() {
        // Codex T2 review B-5: a model with `moved_from_app =
        // "billing"` whose table existed in the before-billing-bucket
        // must produce a single MoveModelBetweenApps op on the
        // destination bucket — NOT a spurious DropTable on billing
        // and AddTable on the new bucket.
        let billing = synth_app("billing", "main");
        let users = synth_app("users", "main");

        let before_model = ModelDescriptor {
            app: Some("billing"),
            ..synth_model("user_settings", "UserSettings")
        };
        let after_model = ModelDescriptor {
            app: Some("users"),
            moved_from_app: Some("billing"),
            ..synth_model("user_settings", "UserSettings")
        };

        let before = project_from_iters(
            [&before_model],
            std::iter::empty::<&EnumDescriptor>(),
            [&billing, &users],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("project before");
        let after = project_from_iters(
            [&after_model],
            std::iter::empty::<&EnumDescriptor>(),
            [&billing, &users],
            "2026-04-25T00:00:00Z".to_string(),
        )
        .expect("project after");

        let deltas = diff_bucket_maps(&before, &after);

        // Source bucket (billing): no spurious DropTable.
        let billing_bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let billing_delta = deltas.iter().find(|d| d.bucket == billing_bucket);
        if let Some(d) = billing_delta {
            assert!(
                !d.operations
                    .iter()
                    .any(|op| matches!(op, SchemaOperation::DropTable(_))),
                "source bucket must not emit DropTable for moved model"
            );
        }

        // Destination bucket (users): MoveModelBetweenApps emitted,
        // no spurious AddTable.
        let users_bucket = BucketKey {
            database: "main".to_string(),
            app: "users".to_string(),
        };
        let users_delta = deltas.iter().find(|d| d.bucket == users_bucket).unwrap();
        assert!(
            users_delta.operations.iter().any(|op| matches!(
                op,
                SchemaOperation::MoveModelBetweenApps { model, from_app, to_app }
                    if model == "user_settings" && from_app == "billing" && to_app == "users"
            )),
            "destination bucket must emit MoveModelBetweenApps"
        );
        assert!(
            !users_delta
                .operations
                .iter()
                .any(|op| matches!(op, SchemaOperation::AddTable(_))),
            "destination bucket must not emit AddTable for moved model"
        );
    }

    #[test]
    fn pk_column_rename_normalises_under_kind_flip() {
        // Codex T2 review B-2 + fixup advisory: a PK column rename
        // concurrent with a kind flip must be recognised as a
        // PkTypeFlip, not Unsupported. The differ runs
        // `diff_columns_in_table` first (which emits `RenameColumn`
        // and returns the rename map) and then `diff_pk_in_table`
        // with that map, normalising the before-PK columns through
        // the rename so the column-list comparison succeeds.
        //
        // We construct two synthetic schemas DIRECTLY (bypassing
        // `project_*`) so we can pin a `Composite(["old_id"])` PK
        // shape on the before side and `Composite(["new_id"])` on
        // the after side with a column rename annotation in
        // between. `project_from_iters` would always synthesise
        // single-column `id` PKs from the descriptor inventory,
        // which doesn't exercise the normalisation code path.
        let bucket = empty_global();

        let mut before = AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        };
        before.models.insert(
            "widgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![ColumnSchema {
                    check: None,
                    default_sql: None,
                    foreign_key: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "old_id".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                }],
                fts: None,
                is_through: false,
                moved_from_app: None,
                partition: None,
                primary_key: PrimaryKeySchema {
                    columns: vec!["old_id".to_string()],
                    kind: PkKindSchema::HeerId,
                },
                rationale: None,
                renamed_from: None,
                rls_enabled: false,
                table: "widgets".to_string(),
                tenant_key: None,
            },
        );

        let mut after = before.clone();
        after.models.insert(
            "widgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![ColumnSchema {
                    name: "new_id".to_string(),
                    renamed_from: Some("old_id".to_string()),
                    ..before.models["widgets"].columns[0].clone()
                }],
                primary_key: PrimaryKeySchema {
                    columns: vec!["new_id".to_string()],
                    kind: PkKindSchema::HeerIdRecencyBiased,
                },
                ..before.models["widgets"].clone()
            },
        );

        let delta = diff_schemas(&before, &after, bucket);

        assert!(
            delta.operations.iter().any(|op| matches!(
                op,
                SchemaOperation::RenameColumn { from, to, .. }
                    if from == "old_id" && to == "new_id"
            )),
            "RenameColumn must be emitted before the PK comparison"
        );
        assert!(
            delta.operations.iter().any(|op| matches!(
                op,
                SchemaOperation::PkTypeFlip {
                    from: PkKindSchema::HeerId,
                    to: PkKindSchema::HeerIdRecencyBiased,
                    ..
                }
            )),
            "PK rename + kind flip must classify as PkTypeFlip, not Unsupported. \
             Operations were: {:?}",
            delta.operations
        );
        assert!(
            !delta
                .operations
                .iter()
                .any(|op| matches!(op, SchemaOperation::Unsupported { .. })),
            "PK rename + kind flip must NOT emit Unsupported"
        );
        // Headline classification is PkTypeFlip (with a co_destructive
        // co-flag of false because no drops are happening).
        assert!(matches!(
            delta.classification,
            Classification::PkTypeFlip {
                co_destructive: false,
                co_lossy: false
            }
        ));
    }

    // ── AddEnumVariant anchor (Codex T3 review B-2) ──────────────────

    /// Build an `AppliedSchema` with a single enum and no models.
    fn schema_with_enum(name: &str, variants: &[&str]) -> AppliedSchema {
        let mut enums = BTreeMap::new();
        enums.insert(
            name.to_string(),
            EnumSchema {
                name: name.to_string(),
                variants: variants.iter().map(|s| s.to_string()).collect(),
            },
        );
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums,
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        }
    }

    #[test]
    fn enum_variant_inserted_at_head_anchors_before_first_existing() {
        // Old: ["b", "c"]. New: ["a", "b", "c"]. The new "a" sits at
        // index 0; the next existing variant in the new list is "b",
        // which is in the old set. Anchor: Before "b".
        let before = schema_with_enum("status", &["b", "c"]);
        let after = schema_with_enum("status", &["a", "b", "c"]);
        let delta = diff_schemas(&before, &after, empty_global());
        let anchor = delta
            .operations
            .iter()
            .find_map(|op| match op {
                SchemaOperation::AddEnumVariant {
                    variant, anchor, ..
                } if variant == "a" => Some(anchor.clone()),
                _ => None,
            })
            .expect("AddEnumVariant for `a`");
        assert_eq!(
            anchor,
            Some(EnumVariantAnchor {
                variant: "b".to_string(),
                kind: EnumVariantAnchorKind::Before,
            })
        );
    }

    #[test]
    fn enum_variant_inserted_in_middle_anchors_before_next_existing() {
        // Old: ["a", "c"]. New: ["a", "b", "c"]. New "b" at index 1;
        // post-anchor "c" exists in old. Anchor: Before "c".
        let before = schema_with_enum("status", &["a", "c"]);
        let after = schema_with_enum("status", &["a", "b", "c"]);
        let delta = diff_schemas(&before, &after, empty_global());
        let anchor = delta
            .operations
            .iter()
            .find_map(|op| match op {
                SchemaOperation::AddEnumVariant {
                    variant, anchor, ..
                } if variant == "b" => Some(anchor.clone()),
                _ => None,
            })
            .expect("AddEnumVariant for `b`");
        assert_eq!(
            anchor,
            Some(EnumVariantAnchor {
                variant: "c".to_string(),
                kind: EnumVariantAnchorKind::Before,
            })
        );
    }

    #[test]
    fn enum_variant_inserted_at_tail_anchors_after_last_existing() {
        // Old: ["a", "b"]. New: ["a", "b", "c"]. New "c" lands at the
        // tail of the new list. No post-anchor exists (nothing follows
        // "c"), so per the priority rule on `pick_enum_variant_anchor`
        // the helper falls back to the nearest pre-anchor — "b", the
        // immediate predecessor that is also in the old set. The
        // emitted DDL is `ALTER TYPE "status" ADD VALUE 'c' AFTER 'b'`.
        //
        // Codex T3 round-2 review N-1: the previous test name
        // (`..._carries_no_anchor`) contradicted the assertion. Pinned
        // the test name to the helper's actual contract — see the
        // doc-comment on `SchemaOperation::AddEnumVariant.anchor`.
        let before = schema_with_enum("status", &["a", "b"]);
        let after = schema_with_enum("status", &["a", "b", "c"]);
        let delta = diff_schemas(&before, &after, empty_global());
        let anchor = delta
            .operations
            .iter()
            .find_map(|op| match op {
                SchemaOperation::AddEnumVariant {
                    variant, anchor, ..
                } if variant == "c" => Some(anchor.clone()),
                _ => None,
            })
            .expect("AddEnumVariant for `c`");
        assert_eq!(
            anchor,
            Some(EnumVariantAnchor {
                variant: "b".to_string(),
                kind: EnumVariantAnchorKind::After,
            })
        );
    }

    #[test]
    fn multiple_new_variants_each_anchor_against_an_existing_neighbour() {
        // Old: ["b"]. New: ["a", "b", "c"]. Insertions: "a" before
        // "b" (post-anchor in old), "c" after "b" (no post-anchor;
        // pre-anchor "b" works).
        let before = schema_with_enum("status", &["b"]);
        let after = schema_with_enum("status", &["a", "b", "c"]);
        let delta = diff_schemas(&before, &after, empty_global());
        let anchor_a = delta
            .operations
            .iter()
            .find_map(|op| match op {
                SchemaOperation::AddEnumVariant {
                    variant, anchor, ..
                } if variant == "a" => Some(anchor.clone()),
                _ => None,
            })
            .expect("AddEnumVariant a");
        let anchor_c = delta
            .operations
            .iter()
            .find_map(|op| match op {
                SchemaOperation::AddEnumVariant {
                    variant, anchor, ..
                } if variant == "c" => Some(anchor.clone()),
                _ => None,
            })
            .expect("AddEnumVariant c");
        assert_eq!(
            anchor_a,
            Some(EnumVariantAnchor {
                variant: "b".to_string(),
                kind: EnumVariantAnchorKind::Before,
            })
        );
        assert_eq!(
            anchor_c,
            Some(EnumVariantAnchor {
                variant: "b".to_string(),
                kind: EnumVariantAnchorKind::After,
            })
        );
    }
}
