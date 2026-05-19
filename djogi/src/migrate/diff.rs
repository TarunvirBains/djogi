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
    AppliedSchema, ColumnSchema, EnumSchema, ExclusionConstraintSchema, ForeignKeySchema,
    GeneratedColumnSchema, IndexSchema, PkKindSchema, TableSchema,
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

    /// Table-level `EXCLUDE` constraint added on an existing table.
    /// Adding an `EXCLUDE` to a populated table classifies as
    /// [`OnlineSafetyClassification::OfflineOnly`] — Pg18 has no
    /// `NOT VALID` for `EXCLUDE`, so the live-migration two-phase
    /// staging pattern is structurally impossible. Empty-table
    /// additions still flow through this variant; the classifier
    /// gates on the row-count probe.
    ///
    /// `EXCLUDE` constraints declared at table-creation time are
    /// emitted inside the [`AddTable`](Self::AddTable) operation's
    /// inline DDL, never as a separate `AddExclusionConstraint`.
    AddExclusionConstraint {
        table: String,
        exclusion: ExclusionConstraintSchema,
    },

    /// Table-level `EXCLUDE` constraint dropped. The full schema is
    /// carried so the down migration can re-create the constraint
    /// without re-walking the descriptor.
    DropExclusionConstraint {
        table: String,
        name: String,
        exclusion: ExclusionConstraintSchema,
    },

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
        /// 3. `None` only on degenerate or malformed inputs where no
        ///    anchor exists in the old list in either direction —
        ///    e.g. every old variant has been concurrently dropped
        ///    (`Unsupported` upstream) OR a malformed snapshot where
        ///    the existing enum's variant list is empty (Postgres
        ///    rejects an empty-variant enum at `CREATE TYPE`, so this
        ///    is malformed-snapshot territory only — the runtime
        ///    enum-creation path also rejects it). In practice
        ///    [`pick_enum_variant_anchor`] returns `None` only on
        ///    these inputs; tail-appends with prior real variants
        ///    always land in case (2).
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
    ///
    /// **Production lifecycle.** The per-table flip op is emitted by
    /// the per-table differ ([`diff_pk_in_table`]). At the bucket-walk
    /// finalisation step ([`diff_bucket_maps`]) every per-table
    /// `PkTypeFlip` is **promoted** into a single
    /// [`SchemaOperation::PkTypeFlipGroup`] for that bucket — the
    /// group payload carries the parent table plus every dependent
    /// child / self-FK / join-table / partitioned-parent shape so T9's
    /// segment planner has the full transitive closure available
    /// without re-walking the schema. The standalone `PkTypeFlip`
    /// variant survives for unit-test reach-in fixtures and for
    /// snapshot-level differ assertions; production callers (compose,
    /// runner) only ever see `PkTypeFlipGroup` after the post-walk
    /// finalisation runs.
    PkTypeFlip {
        table: String,
        from: PkKindSchema,
        to: PkKindSchema,
    },

    /// Aggregated PK-type flip across one parent table and every
    /// dependent child whose FK references the parent's PK. Emitted
    /// by [`diff_bucket_maps`] from per-table [`PkTypeFlip`] ops at
    /// finalisation time. T9's segment planner consumes this single
    /// op to emit the multi-stage flip plan (preparation, autofill
    /// trigger install, backfill, concurrent unique index, NOT NULL
    /// proof, cutover) verbatim from the HeeRanjID playbook.
    ///
    /// **Why a group, not per-table.** The cutover transaction in
    /// the playbook (§4 / §6 / §7 / §9) is **one atomic Postgres
    /// transaction across parent and every child**. Without the
    /// grouping the segment planner would emit one isolated cutover
    /// per table and the shared atomic invariant would not hold.
    ///
    /// **Cycles + self-FK + join-table + partitioned** — the group
    /// records each via a typed sub-shape so the planner can switch
    /// on the playbook section that applies. A single delta typically
    /// carries one group per migrating parent; multi-parent flips
    /// (e.g. `tags` and `authors` migrating together) emit one group
    /// per parent and the operator composes them as one migration.
    PkTypeFlipGroup(PkTypeFlipGroup),

    /// Merged multi-parent PK-type flip — emitted when two or more
    /// parents share a cross-flipping join table (both parents
    /// flipping in the same delta AND a junction whose two FKs each
    /// reference one parent) AND
    /// [`PkFlipJoinTableOption::OptionA`] is in effect. Per playbook
    /// §7 (line 327 of `asc-to-desc.md`) Option A is the
    /// **stage-interleaved** mega-tx layout: all parents prepare in
    /// one transaction, all backfill in one segment, all index in
    /// one segment, all NOT-VALID-FK in one segment, all NOT-NULL-
    /// proof in one segment, and **one** atomic cutover transaction
    /// re-points every FK column on the join table at the new
    /// `id_desc` columns of every parent.
    ///
    /// **Why not just emit several `PkTypeFlipGroup`s back-to-back.**
    /// The segment planner used to lower each `PkTypeFlipGroup` as a
    /// full 5-segment plan and concatenate them. With a cross-
    /// flipping join table that scheme is incorrect — the winner's
    /// segment 3b emits `... FOREIGN KEY (tag_id_desc) REFERENCES
    /// jt_tags(id_desc) NOT VALID` against `jt_tags(id_desc)` that
    /// the loser's segment 1 has not yet created. Postgres rejects
    /// the migration mid-apply. This multi-parent variant is the
    /// structural fix.
    ///
    /// **Construction.** [`apply_pk_flip_join_table_option`] under
    /// Option A merges every cross-flipping cluster of single-parent
    /// `PkTypeFlipGroup` ops into one `PkTypeFlipMultiGroup` carrying
    /// the participating groups in alphabetical-by-parent order. The
    /// "winner takes ownership of the join table" rule still applies
    /// inside the merged group: only the alphabetically smallest
    /// parent's `join_tables` list keeps the cross-flipping entry
    /// (so the join table's shadow columns / triggers / indexes are
    /// installed once, not twice). Non-cross-flipping flips (no
    /// shared join table, or Option B) remain plain
    /// `PkTypeFlipGroup` ops.
    ///
    /// **Lowering.** [`crate::migrate::pk_flip::build_segments_multi`]
    /// builds a single 5-segment (or 7-segment when partitioned or
    /// FK-cascading) plan that, at each stage, emits every parent's
    /// stage-N statements in alphabetical order. The cutover (final
    /// segment) is one transaction; every parent's drop / rename /
    /// add-constraint statements run inside it.
    PkTypeFlipMultiGroup(Vec<PkTypeFlipGroup>),

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

    /// Set / change / clear the table-level `COMMENT ON TABLE`
    /// metadata for `table`. Phase 8.5 Cluster 4 (djogi#217).
    ///
    /// Carries both `from` and `to` so the SQL emitter renders a
    /// fully reversible down side — `from = Some(prev)` restores the
    /// pre-operation comment, `from = None` rolls back to the
    /// commentless state with `COMMENT ON TABLE … IS NULL`. The
    /// differ filters no-op pairs (`(None, None)` and
    /// `(Some(a), Some(b))` with `a == b`) before emission. The op
    /// classifies as catalog-only (`OnlineSafe`) — `COMMENT ON` is a
    /// pure `pg_description` write with no row touch.
    SetTableComment {
        /// Table whose comment is being set / changed / cleared.
        table: String,
        /// Prior `COMMENT ON TABLE` value. `None` when no comment
        /// existed before the operation.
        from: Option<String>,
        /// Target `COMMENT ON TABLE` value. `None` clears the
        /// comment (lowered to `COMMENT ON TABLE … IS NULL`).
        to: Option<String>,
    },

    /// Set / change / clear table storage parameters. Phase 8.5
    /// Cluster 4 (djogi#218).
    SetStorageParams {
        table: String,
        from: Option<String>,
        to: Option<String>,
    },

    /// Set / change / clear the explicit table tablespace. Phase 8.5
    /// Cluster 4 (djogi#219).
    SetTablespace {
        table: String,
        from: Option<String>,
        to: Option<String>,
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
    /// clause is needed, plus the optional adopter-supplied `using`
    /// expression for non-default cast paths (djogi#220).
    ///
    /// **`using` semantics.** `Some(expr)` is sourced from the AFTER
    /// column's `#[field(type_change_using = "<expr>")]`; the SQL
    /// emitter inlines the expression verbatim into the emitted
    /// `ALTER COLUMN … TYPE … USING (<expr>)` statement so adopters
    /// can perform non-default casts (e.g. `TEXT → UUID`) that
    /// Postgres refuses to convert automatically. `None` falls back to
    /// the framework's default `USING <col>::<new_type>` cast, which
    /// works for every pair Postgres accepts implicitly (e.g. widenings
    /// like `INTEGER → BIGINT`). The down-side rollback always uses
    /// the default cast — symmetric down-side USING expressions are
    /// not modelled because the rollback path is operator-owned in
    /// practice (the adopter hand-edits the emitted down SQL when
    /// the inverse cast also needs special handling).
    ChangeType {
        from: String,
        to: String,
        /// Adopter-supplied `USING` expression for the forward
        /// (apply) direction. `None` when the field carried no
        /// `#[field(type_change_using = "...")]` attribute.
        using: Option<String>,
    },

    /// `SET / DROP CHECK` constraint at the column level.
    ///
    /// Carries **both** the prior CHECK expression (`from`) and the new
    /// CHECK expression (`to`) so the SQL emitter can render a fully
    /// reversible down-migration. Without `from`, a `DROP CHECK`
    /// rollback would have no way to restore the original constraint
    /// — the same lossy-rollback gap GPT-5.5 review flagged for type
    /// migrations on checked columns.
    ///
    /// Variant semantics — `(from, to)` pair:
    ///
    /// - `(None, None)` — never emitted; the differ filters no-op.
    /// - `(Some(b), Some(a))` with `b == a` — never emitted; differ
    ///   filters identical.
    /// - `(None, Some(expr))` — ADD CHECK. Up: `ADD CONSTRAINT ...
    ///   CHECK (expr)`. Down: `DROP CONSTRAINT ...`.
    /// - `(Some(prior), None)` — DROP CHECK. Up: `DROP CONSTRAINT ...`.
    ///   Down: `ADD CONSTRAINT ... CHECK (prior)` — fully recoverable.
    /// - `(Some(b), Some(a))` with `b != a` — currently the differ
    ///   splits AMEND into two emissions: `(Some(b), None)` then
    ///   `(None, Some(a))`. The SQL emitter handles the merged form
    ///   too (DROP+ADD in one statement pair) for callers that may
    ///   want it later.
    SetCheck {
        /// Prior CHECK expression (the constraint already on the column).
        /// `None` when no CHECK existed before the operation.
        from: Option<String>,
        /// Target CHECK expression (the constraint after the operation).
        /// `None` to drop the constraint without replacement.
        to: Option<String>,
    },

    /// Column-level `UNIQUE` constraint flipped.
    SetUnique(bool),

    /// `#[field(index)]` flag flipped (column-level implicit index).
    SetIndexed(bool),

    /// `GENERATED ALWAYS AS (<expr>) STORED` declaration changed.
    /// `from = None, to = Some(_)` adds a generated expression to a
    /// regular column; `from = Some(_), to = None` strips it; both
    /// `Some` with differing expressions is an expression change.
    /// All cases on populated tables classify as `OfflineOnly` because
    /// Postgres rewrites every row under `AccessExclusiveLock` to
    /// materialise / clear the stored expression. The empty-table
    /// case flows through the regular fast-path.
    SetGenerated {
        from: Option<GeneratedColumnSchema>,
        to: Option<GeneratedColumnSchema>,
    },

    /// Set / change / clear the column-level `COMMENT ON COLUMN`
    /// metadata. djogi#217.
    ///
    /// Carries both `from` and `to` so the SQL emitter renders a
    /// fully reversible down side — `from = Some(prev)` restores the
    /// pre-operation comment, `from = None` rolls back to the
    /// commentless state with `COMMENT ON COLUMN … IS NULL`. The
    /// differ filters no-op pairs (`(None, None)` and
    /// `(Some(a), Some(b))` with `a == b`) before emission.
    SetComment {
        /// Prior `COMMENT ON COLUMN` value. `None` when no comment
        /// existed before the operation.
        from: Option<String>,
        /// Target `COMMENT ON COLUMN` value. `None` clears the
        /// comment (lowered to `COMMENT ON COLUMN … IS NULL`).
        to: Option<String>,
    },

    /// Identity-column declaration changed
    /// (`GENERATED BY DEFAULT / ALWAYS AS IDENTITY`). Cluster E #86
    /// fix + Codex T22 BLOCK-3 closure.
    ///
    /// Transitions:
    /// - `from = None, to = Some(kind)` — add identity to existing
    ///   column. Lowered to
    ///   `ALTER TABLE t ALTER COLUMN c ADD <kind sql_clause>`.
    /// - `from = Some(_), to = None` — drop identity. Lowered to
    ///   `ALTER TABLE t ALTER COLUMN c DROP IDENTITY`.
    /// - `from = Some(a), to = Some(b)` (a ≠ b) — kind change
    ///   (BY DEFAULT ↔ ALWAYS). Lowered to
    ///   `ALTER TABLE t ALTER COLUMN c SET GENERATED <kind>`.
    ///
    /// Identity columns are sequence-backed at the Postgres level;
    /// the ADD migration triggers Postgres's own sequence allocation
    /// and starts the sequence after MAX(c) for existing rows. No
    /// Djogi-side backfill needed.
    SetIdentity {
        from: Option<crate::migrate::schema::IdentityKindSchema>,
        to: Option<crate::migrate::schema::IdentityKindSchema>,
    },
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

/// Aggregated PK-type-flip group payload — see
/// [`SchemaOperation::PkTypeFlipGroup`] for the lifecycle contract.
///
/// **Determinism.** Every collection inside this struct is sorted
/// (table names alphabetically; column pairs by source column name)
/// so two runs of the differ produce byte-identical output. The
/// segment planner depends on this — its emitted SQL must be
/// reproducible across runs to satisfy the byte-equality regression
/// tests against the HeeRanjID playbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkTypeFlipGroup {
    /// Postgres table name of the parent whose PK is flipping.
    pub parent_table: String,
    /// Source PK kind (e.g. `HeerId`).
    pub parent_from: PkKindSchema,
    /// Target PK kind (e.g. `HeerIdRecencyBiased`).
    pub parent_to: PkKindSchema,
    /// Direction of the flip — derived from `parent_from` /
    /// `parent_to` for convenience. Asc→Desc uses `heerid_to_desc` /
    /// `ranjid_to_desc`; Desc→Asc uses `heerid_to_asc` / `ranjid_to_asc`.
    pub direction: PkFlipDirection,
    /// Children with a single ascending-side FK pointing at
    /// `parent_table.id`. Each child gets its own `_desc` shadow
    /// column, NOT-VALID FK, backfill, index — all assembled into
    /// the shared cutover transaction. Sorted by table name then
    /// FK column for determinism.
    pub children: Vec<PkFlipChild>,
    /// Self-FK pairs — when `parent_table` has at least one FK
    /// pointing back at itself, those pairs migrate alongside the PK
    /// via a single multi-pair trigger install. `None` when the
    /// parent has no self-FK; when present, the `(src, dst)` list
    /// always begins with `("id", "id_desc")` followed by each
    /// self-FK column in alphabetical order.
    pub self_fk: Option<PkFlipSelfFk>,
    /// Join tables — junction tables whose `is_through` flag is set
    /// AND whose two FK columns both point at this parent (rare —
    /// most join tables span two parents) OR participate via
    /// `join_table_partner`. Drives §7 of the playbook.
    pub join_tables: Vec<PkFlipJoinTable>,
    /// Cycles — pairs of tables with mutual FKs. Each cycle entry
    /// records the peer table name; the planner emits both FKs as
    /// `DEFERRABLE INITIALLY DEFERRED` and prefixes the cutover with
    /// `SET CONSTRAINTS ALL DEFERRED`. Sorted alphabetically.
    pub cycles: Vec<PkFlipCycle>,
    /// Partitioned-parent metadata — `Some(...)` when `parent_table`
    /// has a `PARTITION BY` declaration. The planner emits the §9
    /// PG 13+ shape (parent-level shadow column, parent-level
    /// trigger, per-partition backfill, per-partition unique index +
    /// parent UNIQUE placeholder + ATTACH, partitioned-parent PK via
    /// `ADD PRIMARY KEY (...)`). Per-partition leaf names are
    /// supplied at runtime via the runner because partition leaves
    /// are live-DB state, not descriptor state. The presence of
    /// `Some` here is the trigger; the runner reads `pg_inherits`
    /// to enumerate leaves at apply time.
    pub partitioned_parent: Option<PkFlipPartitionedMeta>,
    /// Co-existing destructive ops in the same delta. Mirrors the
    /// per-delta `co_destructive` flag — duplicated here so the SQL
    /// emitter / runner can decide gating without re-walking the
    /// delta classification.
    pub co_destructive: bool,
    /// Co-existing lossy ops in the same delta.
    pub co_lossy: bool,
    /// Join-table cutover layout — `OptionA` (default) emits one
    /// shared cutover transaction across both parents and the join
    /// table per playbook §7; `OptionB` splits the cutover into
    /// sequential per-parent migrations. Set by the differ from
    /// [`crate::config::MigrateConfig::pk_flip_join_table_option`]
    /// at delta-construction time so the planner reads the operator-
    /// chosen layout straight from the group without needing the
    /// `MigrateConfig` plumbed through to lowering. The default
    /// value is `OptionA`; the differ overrides it per the operator
    /// config when the delta is built. **Behaviourally,** Option B
    /// emits a smaller cutover statement set per group — the join
    /// table's partner-side FK statements are deferred to the next
    /// flip group's cutover instead of bundled into this one.
    pub join_table_option: PkFlipJoinTableOption,
}

/// Join-table cutover layout. Mirrors the operator-facing knob in
/// [`crate::config::MigrateConfig::pk_flip_join_table_option`]
/// (which carries an ASCII `'A'` / `'B'` because TOML doesn't have
/// a strongly-typed enum). The differ converts the config char to
/// this enum once and writes it onto every emitted
/// [`PkTypeFlipGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkFlipJoinTableOption {
    /// Single mega-transaction covers both parents + the join table
    /// in one cutover (playbook §7 default).
    OptionA,
    /// Sequential per-parent flips. The current flip group's cutover
    /// only re-points `fk_to_parent_column`; the partner FK on the
    /// same join table is left for the second parent's flip group
    /// to re-point in its own cutover.
    OptionB,
}

impl PkFlipJoinTableOption {
    /// Parse from the [`crate::config::MigrateConfig`] char value.
    /// Anything other than `'B'` (case-insensitive) maps to
    /// [`PkFlipJoinTableOption::OptionA`] — keeping the unknown-
    /// value fallback aligned with the safer default.
    pub fn from_config_char(c: char) -> Self {
        match c {
            'B' | 'b' => Self::OptionB,
            _ => Self::OptionA,
        }
    }
}

/// Walk a list of [`SchemaDelta`]s and overwrite every emitted
/// [`PkTypeFlipGroup`]'s `join_table_option` with the operator-
/// configured value.
///
/// **When to call.** After [`diff_bucket_maps`] has produced the
/// per-bucket deltas and before the segment planner is invoked.
/// The compose pipeline + `db reset` replay both call this helper
/// so the operator's TOML choice reaches the planner.
///
/// **Idempotent.** Calling twice with the same option is a no-op.
///
/// **Multi-parent merge.** Under
/// [`PkFlipJoinTableOption::OptionA`] the function does more than
/// stamp the option — it MERGES every cluster of cross-flipping
/// `PkTypeFlipGroup` ops in a delta into a single
/// [`SchemaOperation::PkTypeFlipMultiGroup`]. The merger is what
/// makes Option A's "single mega-transaction" semantics structurally
/// realisable. Without merging, the segment planner lowers each
/// group as a back-to-back 5-segment plan, and the winner's segment
/// 3b emits `... FOREIGN KEY (tag_id_desc) REFERENCES
/// jt_tags(id_desc) NOT VALID` against a partner shadow column that
/// only lands in the loser's segment 1 — Postgres rejects the
/// migration mid-apply. The merged variant replaces N back-to-back
/// 5-segment plans with ONE stage-interleaved 5-segment plan that
/// emits every parent's stage-N statements together. Cluster
/// detection uses Union-Find over the join-table peer relation
/// (parents A and B share a cluster when at least one
/// `PkFlipJoinTable` in either group references the other parent
/// via `fk_to_partner_column = Some(_)` AND
/// `fk_to_partner_table = Some(<peer>)`).
pub fn apply_pk_flip_join_table_option(deltas: &mut [SchemaDelta], option: PkFlipJoinTableOption) {
    // First pass: stamp the option onto every emitted group.
    for delta in deltas.iter_mut() {
        for op in &mut delta.operations {
            if let SchemaOperation::PkTypeFlipGroup(group) = op {
                group.join_table_option = option;
            } else if let SchemaOperation::PkTypeFlipMultiGroup(groups) = op {
                // Idempotent re-stamp — `apply_pk_flip_join_table_option`
                // may run more than once during the compose pipeline
                // (e.g. tests that toggle the option). Stamp every
                // member group inside an existing multi-group too.
                for g in groups.iter_mut() {
                    g.join_table_option = option;
                }
            }
        }
    }
    // Under Option B leave the per-parent groups in place — the
    // sequential-per-parent layout each cutover only touches its
    // own FK column on the join table. Per-group `join_tables`
    // lists keep the cross-flipping entry on BOTH sides so each
    // group's `jt_shadow_pairs` returns the parent's pair and the
    // cutover finalises only that side of the join table; the
    // partner pair survives until the partner parent's group runs.
    if option != PkFlipJoinTableOption::OptionA {
        return;
    }
    // Under Option A, find every cross-flipping cluster of groups
    // and merge each cluster (of 2+ groups) into a
    // `PkTypeFlipMultiGroup` so the segment planner can lower the
    // cluster as ONE stage-interleaved 5-segment plan. Single-
    // parent groups (no cross-flipping peer) stay as plain
    // `PkTypeFlipGroup` ops — the back-to-back layout is correct
    // for them because there is no partner shadow column to
    // reference.
    for delta in deltas.iter_mut() {
        merge_cross_flipping_groups_into_multi(delta).expect(
            "partitioned multi-parent clusters must already be rejected by diff_bucket_maps",
        );
    }
}

pub(crate) fn partitioned_multi_parent_cluster_error(
    groups: &[PkTypeFlipGroup],
) -> Option<DiffError> {
    if groups.len() < 2 {
        return None;
    }
    let mut partitioned_parents: Vec<String> = groups
        .iter()
        .filter(|g| g.partitioned_parent.is_some())
        .map(|g| g.parent_table.clone())
        .collect();
    if partitioned_parents.is_empty() {
        return None;
    }
    partitioned_parents.sort();
    partitioned_parents.dedup();

    let mut cross_flipping_partners: Vec<String> =
        groups.iter().map(|g| g.parent_table.clone()).collect();
    cross_flipping_partners.sort();
    cross_flipping_partners.dedup();

    Some(DiffError::PartitionedMultiParentClusterUnsupported {
        partitioned_parents,
        cross_flipping_partners,
    })
}

fn cluster_pk_flip_groups(ops: &[SchemaOperation]) -> BTreeMap<String, Vec<&PkTypeFlipGroup>> {
    let groups: Vec<&PkTypeFlipGroup> = ops
        .iter()
        .filter_map(|op| match op {
            SchemaOperation::PkTypeFlipGroup(group) => Some(group),
            _ => None,
        })
        .collect();

    let mut peer_edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut rep: BTreeMap<String, String> = BTreeMap::new();
    for group in &groups {
        rep.insert(group.parent_table.clone(), group.parent_table.clone());
        for jt in &group.join_tables {
            if let Some(partner) = jt.fk_to_partner_table.as_deref() {
                let a = group.parent_table.clone();
                let b = partner.to_string();
                let edge = if a <= b { (a, b) } else { (b, a) };
                peer_edges.insert(edge);
            }
        }
    }
    if peer_edges.is_empty() {
        return BTreeMap::new();
    }

    fn find(rep: &mut BTreeMap<String, String>, x: &str) -> String {
        let mut cur = x.to_string();
        loop {
            let parent = rep.get(&cur).cloned().unwrap_or_else(|| cur.clone());
            if parent == cur {
                return cur;
            }
            let grand = rep.get(&parent).cloned().unwrap_or_else(|| parent.clone());
            rep.insert(cur.clone(), grand.clone());
            cur = grand;
        }
    }

    for (a, b) in &peer_edges {
        let ra = find(&mut rep, a);
        let rb = find(&mut rep, b);
        if ra != rb {
            let (winner, loser) = if ra <= rb { (ra, rb) } else { (rb, ra) };
            rep.insert(loser, winner);
        }
    }

    let mut clusters: BTreeMap<String, Vec<&PkTypeFlipGroup>> = BTreeMap::new();
    for group in groups {
        let root = find(&mut rep, &group.parent_table);
        clusters.entry(root).or_default().push(group);
    }
    clusters
}

fn reject_partitioned_multi_parent_clusters(ops: &[SchemaOperation]) -> Result<(), DiffError> {
    let clusters = cluster_pk_flip_groups(ops);

    for (_root, mut cluster) in clusters {
        if cluster.len() < 2 {
            continue;
        }
        cluster.sort_by(|a, b| a.parent_table.cmp(&b.parent_table));
        let member_set: BTreeSet<String> = cluster.iter().map(|g| g.parent_table.clone()).collect();
        let has_internal_cross_flip = cluster.iter().any(|group| {
            group.join_tables.iter().any(|jt| {
                jt.fk_to_partner_table
                    .as_ref()
                    .is_some_and(|partner| member_set.contains(partner))
            })
        });
        if !has_internal_cross_flip {
            continue;
        }
        let cluster_groups: Vec<PkTypeFlipGroup> = cluster.into_iter().cloned().collect();
        if let Some(err) = partitioned_multi_parent_cluster_error(&cluster_groups) {
            return Err(err);
        }
    }

    Ok(())
}

/// Errors the differ surfaces.
///
/// Distinct from [`super::sql::SqlEmitError`] — `DiffError`
/// reports failures that occur BEFORE SQL emission, during the
/// per-bucket walk and the PK-flip group promotion. Every variant
/// carries enough context for an actionable operator message.
///
/// # B-4r — panic → Result migration (Codex round-3)
///
/// [`promote_pk_flips_to_groups`] previously panicked on the
/// depth-65 contract violation. The panic prevented compose / build
/// from surfacing a structured error to the operator and broke the
/// general principle that the differ never panics on user-shaped
/// input. This enum carries the chain of tables that drove the
/// blow-out so the operator can identify the offending FK cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffError {
    /// The PK-flip transitive FK closure walked more levels than
    /// [`promote_pk_flips_to_groups`]'s `MAX_CLOSURE_DEPTH` allows.
    /// The graph is pathological — likely an unbounded fan-out or
    /// a cycle the visited-set protection failed to short-circuit
    /// in time. The differ refuses to compose a migration over a
    /// graph it cannot reason about.
    PkFlipCascadeDepthExceeded {
        /// Postgres table name of the migrating parent that rooted
        /// the closure walk.
        parent_table: String,
        /// Trail of one representative table per depth level. The
        /// first entry is `parent_table`; subsequent entries are
        /// one (alphabetically-first) member of each next-frontier
        /// pass. Operators read this to identify the FK shape that
        /// triggered the depth blow-out.
        chain: Vec<String>,
        /// The maximum depth the closure was allowed to walk.
        /// Captured here so the operator-facing message can render
        /// the contract limit alongside the chain.
        max_depth: u32,
    },
    /// A cross-flipping cluster includes one or more partitioned
    /// parents. Phase 7 rejects this shape because the multi-parent
    /// lowerer cannot safely interleave partitioned cutovers with
    /// partner-referencing join-table work.
    PartitionedMultiParentClusterUnsupported {
        /// Every partitioned parent in the cluster.
        partitioned_parents: Vec<String>,
        /// Every parent participating in the cross-flipping cluster.
        cross_flipping_partners: Vec<String>,
    },
    /// A `PkTypeFlipGroup` reached the lowering path with malformed
    /// self-FK metadata (sidecar vector lengths out of sync). Codex
    /// round-7 BLOCK 3: surface the underlying [`PkFlipError`] so the
    /// segment planner and `build_segments_multi` single-member
    /// fallback never bypass `validate_group`.
    PkFlipMalformedSelfFkMetadata(super::pk_flip::PkFlipError),
}

impl From<super::pk_flip::PkFlipError> for DiffError {
    fn from(err: super::pk_flip::PkFlipError) -> Self {
        DiffError::PkFlipMalformedSelfFkMetadata(err)
    }
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffError::PkFlipCascadeDepthExceeded {
                parent_table,
                chain,
                max_depth,
            } => write!(
                f,
                "PK-flip transitive FK closure exceeded {max_depth} levels rooted at \
                 {parent_table}; table_chain={chain:?}; graph likely has a pathological \
                 cycle or unbounded fan-out — refusing to compose the migration",
            ),
            DiffError::PkFlipMalformedSelfFkMetadata(err) => {
                write!(f, "diff lowering rejected a malformed PK-flip group: {err}")
            }
            DiffError::PartitionedMultiParentClusterUnsupported {
                partitioned_parents,
                cross_flipping_partners,
            } => write!(
                f,
                "PK-flip cross-flipping cluster mixes partitioned parents \
                 {partitioned_parents:?} with partners {cross_flipping_partners:?}; \
                 Phase 7 rejects this unsupported multi-parent partitioned shape",
            ),
        }
    }
}

impl std::error::Error for DiffError {}

/// Codex round-4 B-15 — merge every cross-flipping cluster of
/// `PkTypeFlipGroup` ops in a single delta into one
/// [`SchemaOperation::PkTypeFlipMultiGroup`].
///
/// **What "cross-flipping" means.** A pair `(A, B)` of
/// `PkTypeFlipGroup`s is cross-flipping when at least one
/// `PkFlipJoinTable` entry in EITHER group records the other parent
/// via `fk_to_partner_table = Some(<peer>)` AND
/// `fk_to_partner_column = Some(_)`. The partner-table field was
/// added in Codex round-3 B-12 specifically so the planner can
/// resolve the cross-flipping shape without re-walking the schema.
///
/// **Cluster construction (Union-Find).** Cross-flipping is
/// transitive — if A↔B share a join table and B↔C share a different
/// join table, the SQL emission needs ALL THREE in one mega-tx
/// because B's segment 3b would otherwise reference shadow columns
/// on both A and C that haven't landed yet. The merger therefore
/// uses Union-Find over the parent-table set, with edges drawn from
/// every cross-flipping `PkFlipJoinTable` entry. Clusters of size
/// 1 (no cross-flipping peer) stay as their original
/// `PkTypeFlipGroup` op; clusters of size 2+ are replaced by ONE
/// `PkTypeFlipMultiGroup` carrying the cluster's groups in
/// alphabetical-by-parent order.
///
/// **Winner-takes-all on join_tables.** Inside a merged cluster the
/// alphabetically smallest parent retains every cross-flipping
/// `PkFlipJoinTable` entry; the other members' `join_tables` are
/// emptied of cross-flipping entries (single-parent join tables —
/// `fk_to_partner_column = None` — survive on whichever group owns
/// them). The lowering walks the multi-group's groups in order and
/// emits each member's stage-N statements at stage N; the winner's
/// stage-N is where the join table's stage-N statements appear, so
/// the join table's shadow column / trigger / index / FK / cutover
/// SQL is emitted exactly once per stage and references both
/// parents' `id_desc` columns that exist by the time stage-3b runs
/// (because every parent prepared in stage 1).
///
/// **Determinism.** Cluster ordering is deterministic — alphabetical
/// by the smallest parent in the cluster. Within a cluster, member
/// groups are alphabetical by `parent_table`. The same input
/// produces the same `PkTypeFlipMultiGroup` shape across runs,
/// preserving the byte-stable SQL guarantee.
fn merge_cross_flipping_groups_into_multi(delta: &mut SchemaDelta) -> Result<(), DiffError> {
    let clusters: BTreeMap<String, Vec<String>> = cluster_pk_flip_groups(&delta.operations)
        .into_iter()
        .map(|(root, members)| {
            (
                root,
                members
                    .into_iter()
                    .map(|group| group.parent_table.clone())
                    .collect(),
            )
        })
        .collect();

    // For each cluster of 2+ parents, drain those `PkTypeFlipGroup`
    // ops out of the operation list, apply the winner-takes-all
    // transfer of cross-flipping `PkFlipJoinTable` entries inside
    // the cluster, and push a single `PkTypeFlipMultiGroup` in
    // their place.
    let mut multi_groups: Vec<Vec<PkTypeFlipGroup>> = Vec::new();
    for (_rep, members) in clusters {
        if members.len() < 2 {
            continue;
        }
        // Stable membership lookup for the retain pass below.
        let mem_set: std::collections::BTreeSet<String> = members.iter().cloned().collect();
        let mut cluster_groups: Vec<PkTypeFlipGroup> = delta
            .operations
            .extract_if(.., |op| {
                matches!(
                    op,
                    SchemaOperation::PkTypeFlipGroup(g) if mem_set.contains(&g.parent_table)
                )
            })
            .filter_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .collect();
        // Sort the cluster's groups alphabetically by parent_table —
        // the lowering depends on this for deterministic stage-N
        // emission order.
        cluster_groups.sort_by(|a, b| a.parent_table.cmp(&b.parent_table));
        if let Some(err) = partitioned_multi_parent_cluster_error(&cluster_groups) {
            return Err(err);
        }

        // Winner-takes-all on cross-flipping join_tables. The
        // alphabetically smallest parent (cluster_groups[0]) keeps
        // every cross-flipping entry. Other members drop the
        // cross-flipping rows from their `join_tables` so the
        // join-table SQL is emitted exactly once per stage. Single-
        // parent (`fk_to_partner_column = None`) entries stay on
        // whichever group originally carried them.
        let cross_flipping_jt_names: std::collections::BTreeSet<String> = cluster_groups
            .iter()
            .flat_map(|g| {
                g.join_tables
                    .iter()
                    .filter(|jt| jt.fk_to_partner_column.is_some())
                    .map(|jt| jt.table.clone())
            })
            .collect();
        for (idx, g) in cluster_groups.iter_mut().enumerate() {
            if idx == 0 {
                // Winner — keep all cross-flipping entries; collect
                // any cross-flipping entries that lived on losers
                // and graft them onto the winner.
                continue;
            }
            g.join_tables.retain(|jt| {
                // Drop only cross-flipping entries shared with the
                // cluster — single-parent entries (no partner col)
                // stay where they are.
                !(jt.fk_to_partner_column.is_some() && cross_flipping_jt_names.contains(&jt.table))
            });
        }
        // Graft any cross-flipping entries that lived only on a
        // loser onto the winner (rare — typically the winner has
        // them already because both sides record the same join-
        // table). Without the graft, the winner would lower the
        // cluster missing one half of the join-table orchestration.
        let winner_existing: std::collections::BTreeSet<String> = cluster_groups[0]
            .join_tables
            .iter()
            .filter(|jt| jt.fk_to_partner_column.is_some())
            .map(|jt| jt.table.clone())
            .collect();
        // Re-collect from non-winner indices BEFORE we drop them
        // (which already happened above).
        // We need to iterate the original cluster's pre-mutation
        // join_tables to find any that should graft; but we already
        // mutated them. So instead, walk the deltas again with the
        // operation freshly drained: the winner_existing set tells
        // us which cross-flipping JTs the winner already covers; if
        // any cross_flipping_jt_names entry isn't in winner_existing,
        // it must have been dropped from a loser — but the loser's
        // entry was structurally symmetrical to the winner's (both
        // sides record the same `(jt, partner)` shape), so this
        // case should not arise in practice. We log and continue
        // rather than silently swallow; an asymmetric pair is a
        // differ bug.
        for jt_name in &cross_flipping_jt_names {
            if !winner_existing.contains(jt_name) {
                // Differ produced an asymmetric cross-flipping pair
                // — one side recorded the partner, the other did
                // not. Defensive: skip the graft (the planner emits
                // the half it can see) rather than panic. Future
                // hardening: a `DiffError` for this case.
                continue;
            }
        }
        multi_groups.push(cluster_groups);
    }

    // Push every multi-group at the END of the operation list so
    // the planner reaches them after every other (single-parent)
    // group. The segment planner already iterates ops in input
    // order, so this produces a deterministic plan.
    for groups in multi_groups {
        delta
            .operations
            .push(SchemaOperation::PkTypeFlipMultiGroup(groups));
    }
    Ok(())
}

/// Direction of a PK-type flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkFlipDirection {
    /// Ascending → descending. Uses `heerid_to_desc` /
    /// `ranjid_to_desc` and `heerid_next_desc()` / `ranjid_next_desc()`
    /// as the new column DEFAULT.
    AscToDesc,
    /// Descending → ascending. Uses `heerid_to_asc` / `ranjid_to_asc`
    /// and `heerid_next()` / `ranjid_next()` as the new column
    /// DEFAULT. The autofill trigger SQL is the symmetric mirror —
    /// `IdKind::Heer.flip_fn()` always returns `heerid_to_desc` so
    /// the reverse-direction emitter substitutes `heerid_to_asc`
    /// directly in the trigger body it generates.
    DescToAsc,
}

/// Which family of generators (HeerId vs RanjId) the flip uses.
/// Derived from the PK kind variants on the group; cached on the
/// child / self-FK / join-table records so the planner can emit
/// the right generator name without re-deriving from the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkFlipFamily {
    /// `BIGINT` PK, `heerid_to_desc` / `heerid_to_asc` /
    /// `heerid_next` / `heerid_next_desc`.
    Heer,
    /// `UUID` PK, `ranjid_to_desc` / `ranjid_to_asc` /
    /// `ranjid_next` / `ranjid_next_desc`.
    Ranj,
}

/// One child table whose FK to the migrating parent must follow the
/// flip into the shared cutover transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkFlipChild {
    /// Postgres table name of the child.
    pub table: String,
    /// FK column on the child (e.g. `"author_id"` for a
    /// `books → authors` relation).
    pub fk_column: String,
    /// FK constraint name as recorded in the live DB. Composed via
    /// `<child_table>_<fk_column>_fkey` (Postgres' default convention
    /// when the schema didn't supply an explicit name); the runtime
    /// drops this exact constraint name during the cutover. The
    /// emitter uses this verbatim so a hand-named constraint that
    /// matches the convention round-trips unchanged.
    pub fk_constraint_name: String,
    /// Original FK cascade discipline, preserved through the cutover.
    pub on_delete: super::schema::OnDeleteSchema,
    /// Original FK deferrability flags. Preserved through the
    /// cutover so a deferrable source FK recreates as deferrable on
    /// the post-cutover column. Cycle peers force `deferrable = true,
    /// initially_deferred = true` regardless of descriptor input —
    /// see [`PkTypeFlipGroup`] docs and playbook §8.
    pub fk_deferrable: bool,
    /// `true` iff the original FK was `INITIALLY DEFERRED`. Only
    /// meaningful when `fk_deferrable = true`.
    pub fk_initially_deferred: bool,
    /// Whether the child's FK column is nullable. Drives the
    /// playbook's §3.3 NULL-tracking invariant choice — nullable FKs
    /// allow NULL on the desc shadow.
    pub fk_nullable: bool,
    /// Whether the child's FK column is unique (enforced). Affects
    /// the index emitted on the desc shadow column (UNIQUE vs
    /// non-unique). Composite UNIQUE constraints on the FK column
    /// are not modelled here; the playbook's §3.4 emits a non-unique
    /// index on FK shadows by default.
    pub fk_unique: bool,
    /// HeerId vs RanjId family — derived from the child's column
    /// SQL type. Mirrors the parent's family in practice.
    pub family: PkFlipFamily,
    /// `true` when this child also participates in a mutual-FK cycle
    /// with the migrating parent (the parent has its own FK pointing
    /// back at this child). Cycle peers are regular children for
    /// every segment (preparation / backfill / concurrent index /
    /// NOT NULL proof / cutover) — the only delta is that their FK
    /// constraint at segment 3b is `DEFERRABLE INITIALLY DEFERRED`,
    /// and the cutover prefixes the body with `SET CONSTRAINTS ALL
    /// DEFERRED` so mid-transaction states are tolerated until the
    /// final `COMMIT`. See playbook §8 (asc-to-desc.md). The
    /// [`PkTypeFlipGroup::cycles`] vec preserves the cycle structure
    /// (peer column pairs) for diagnostics + cycle-detection in the
    /// emitter; this flag drives per-child SQL shape.
    pub cycle_flag: bool,
}

/// Self-FK metadata for a parent that references itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkFlipSelfFk {
    /// Self-FK columns on the parent. Each entry is the FK column
    /// name (e.g. `"parent_id"` for a `nodes(parent_id REFERENCES
    /// nodes(id))`). Sorted alphabetically.
    pub fk_columns: Vec<String>,
    /// FK constraint names matching `fk_columns` index-for-index.
    pub fk_constraint_names: Vec<String>,
    /// Per-FK deferrability. Index-for-index with `fk_columns` /
    /// `fk_constraint_names`. Cycle path forces `true / true`.
    pub fk_deferrable: Vec<bool>,
    /// Per-FK `INITIALLY DEFERRED` flag. Same indexing as
    /// `fk_deferrable`. Only meaningful when the matching
    /// `fk_deferrable` entry is `true`.
    pub fk_initially_deferred: Vec<bool>,
}

/// Join-table metadata for a many-to-many junction whose two FK
/// columns reference the migrating parent (or one parent + one
/// non-migrating peer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkFlipJoinTable {
    /// Postgres table name of the join table.
    pub table: String,
    /// The FK column on this join table that points at the migrating
    /// parent. Always populated.
    pub fk_to_parent_column: String,
    /// FK constraint name for `fk_to_parent_column`.
    pub fk_to_parent_constraint: String,
    /// Original deferrability of the parent-side FK. Preserved
    /// through the cutover; defaults to `false` for non-deferrable
    /// FKs.
    pub fk_to_parent_deferrable: bool,
    /// `true` iff the parent-side FK was `INITIALLY DEFERRED`.
    /// Only meaningful when `fk_to_parent_deferrable = true`.
    pub fk_to_parent_initially_deferred: bool,
    /// The other FK column on this join table — either points at a
    /// second migrating parent (`Some(...)`) or at a non-migrating
    /// peer (`None`). When `Some`, the join table participates in
    /// **both** parents' migrations and the planner emits the
    /// multi-pair shadow-column install per §7 of the playbook.
    pub fk_to_partner_column: Option<String>,
    /// FK constraint name for `fk_to_partner_column`. `None` when
    /// `fk_to_partner_column` is `None`.
    pub fk_to_partner_constraint: Option<String>,
    /// Postgres table name the partner FK column references.
    /// `Some(_)` exactly when `fk_to_partner_column` is `Some(_)`
    /// — the differ records both fields atomically so the planner
    /// can re-emit the partner's FK constraint targeting the
    /// correct parent table during the cutover. Required under
    /// Option A so the cutover's `ADD CONSTRAINT` statement points
    /// at the right partner table; `None` means single-parent join
    /// where this planner only ever emits the parent-side FK.
    pub fk_to_partner_table: Option<String>,
    /// Original deferrability of the partner-side FK. `false` for
    /// `None` partner. Preserved through the cutover.
    pub fk_to_partner_deferrable: bool,
    /// `true` iff the partner-side FK was `INITIALLY DEFERRED`.
    /// Only meaningful when `fk_to_partner_deferrable = true`.
    pub fk_to_partner_initially_deferred: bool,
    /// Family of the migrating parent's PK.
    pub family: PkFlipFamily,
}

/// One peer in an FK cycle that is migrating alongside this parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkFlipCycle {
    /// Postgres table name of the cycle peer.
    pub peer_table: String,
    /// FK column on `peer_table` pointing at this parent.
    pub peer_fk_column: String,
    /// FK column on this parent pointing at `peer_table`.
    pub self_fk_column: String,
}

/// Partitioned-parent metadata, set when the parent table has a
/// `PARTITION BY` declaration in its descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkFlipPartitionedMeta {
    /// Partition strategy as recorded in the schema snapshot.
    pub partition: super::schema::PartitionSchema,
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
) -> Result<Vec<SchemaDelta>, DiffError> {
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

        // T9 finalisation: aggregate every per-table `PkTypeFlip` op
        // into a `PkTypeFlipGroup` enriched with FK cascade /
        // self-FK / join-table / cycle / partition metadata. The
        // grouping uses the new schema (`a`) as the source of truth
        // for FK relations because that is the post-migration shape
        // every child has to align with — pulling FK metadata from
        // `before` would miss children that gained their FK in this
        // same migration (rare but legal).
        // B-4r (Codex round-3): closure depth blow-out propagates
        // as a structured `DiffError` rather than panicking — the
        // caller (compose / build) renders the chain in the
        // operator-facing error message.
        promote_pk_flips_to_groups(a, &mut delta.operations)?;
        reject_partitioned_multi_parent_clusters(&delta.operations)?;

        // Re-classify after the suppression + emission step.
        delta.classification = classify(&delta.operations);
        out.push(delta);
    }
    Ok(out)
}

/// Walk a delta's operation list, find every per-table
/// [`SchemaOperation::PkTypeFlip`], and promote them to
/// [`SchemaOperation::PkTypeFlipGroup`] with full FK cascade /
/// self-FK / join-table / cycle / partition metadata.
///
/// **In-place rewrite.** The per-table flip op is removed; the new
/// group op is pushed onto the operation list (after every other op
/// to preserve the existing ordering invariants — the segment
/// planner will hoist the group into its own dedicated multi-segment
/// plan regardless of position). When no flip is present the fn is a
/// pure no-op.
///
/// **Determinism.** Children, self-FK columns, join tables, and
/// cycles are sorted alphabetically before being attached to the
/// group so the byte-equality regression tests against the playbook
/// SQL are reproducible run-to-run.
fn promote_pk_flips_to_groups(
    after: &AppliedSchema,
    ops: &mut Vec<SchemaOperation>,
) -> Result<(), DiffError> {
    // Collect parents — `(table, from, to)` for every PkTypeFlip op.
    let mut parents: Vec<(String, PkKindSchema, PkKindSchema)> = Vec::new();
    ops.retain(|op| match op {
        SchemaOperation::PkTypeFlip { table, from, to } => {
            parents.push((table.clone(), from.clone(), to.clone()));
            false
        }
        _ => true,
    });
    if parents.is_empty() {
        return Ok(());
    }
    // Migrating-parent set — used by join-table detection (a join
    // table's "partner" FK column points at another migrating parent
    // when both tables are flipping in this delta). Owned strings
    // because we move `parents` below.
    let migrating_parents: BTreeSet<String> = parents.iter().map(|(t, _, _)| t.clone()).collect();

    // Helper: which family does this PK kind belong to?
    fn family_for_kind(k: &PkKindSchema) -> Option<PkFlipFamily> {
        match k {
            PkKindSchema::HeerId | PkKindSchema::HeerIdRecencyBiased => Some(PkFlipFamily::Heer),
            PkKindSchema::RanjId | PkKindSchema::RanjIdRecencyBiased => Some(PkFlipFamily::Ranj),
            _ => None,
        }
    }

    // B-4: transitive FK closure.
    //
    // For each migrating parent we collect every table whose FK
    // (directly or transitively) ranges over the parent's `id`
    // column's value space. The closure terminates because the FK
    // graph is finite and the worklist only ever grows by direct
    // children of an already-collected table.
    //
    // **Why fixed-point is conservative for asc↔desc flips.** The
    // asc↔desc value distribution shift only re-keys the parent's
    // own PK column. A grandchild that points at a CHILD's `id` is
    // unaffected — C's `id` does not change in P's group. The
    // fixed-point therefore stabilises after one pass for
    // asc↔desc; the loop is here so future PK-type variants that
    // DO require transitive shadow columns (e.g., a hypothetical
    // BIGINT → TEXT key migration) inherit the closure for free.
    //
    // **Cycle protection.** The worklist tracks visited tables in a
    // `BTreeSet` so a cycle of arbitrary length cannot loop the
    // closure indefinitely.
    for (parent_table, parent_from, parent_to) in parents {
        let direction = match (&parent_from, &parent_to) {
            (PkKindSchema::HeerId, PkKindSchema::HeerIdRecencyBiased)
            | (PkKindSchema::RanjId, PkKindSchema::RanjIdRecencyBiased) => {
                PkFlipDirection::AscToDesc
            }
            (PkKindSchema::HeerIdRecencyBiased, PkKindSchema::HeerId)
            | (PkKindSchema::RanjIdRecencyBiased, PkKindSchema::RanjId) => {
                PkFlipDirection::DescToAsc
            }
            // Should never happen — `is_pk_kind_flip` gates the
            // PkTypeFlip emission to only the four supported pairs.
            // Pin to AscToDesc as a defensive default; the planner
            // will surface a hard error if the parent kinds are
            // inconsistent with the rest of the group.
            _ => PkFlipDirection::AscToDesc,
        };
        let family = family_for_kind(&parent_from).unwrap_or(PkFlipFamily::Heer);

        // Children: every other table whose column.foreign_key
        // points at `parent_table`. Skip the parent itself (handled
        // via self_fk).
        let mut children: Vec<PkFlipChild> = Vec::new();
        let mut self_fk_cols: Vec<String> = Vec::new();
        let mut self_fk_constraints: Vec<String> = Vec::new();
        // Codex round-4 B-16: per-self-FK deferrability flags
        // populated index-for-index alongside `self_fk_cols` /
        // `self_fk_constraints`. Cycle path forces both to `true`
        // — see the conditional at the end of the population loop.
        let mut self_fk_deferrable: Vec<bool> = Vec::new();
        let mut self_fk_initially_deferred: Vec<bool> = Vec::new();
        let mut join_tables: Vec<PkFlipJoinTable> = Vec::new();
        let mut cycles: Vec<PkFlipCycle> = Vec::new();

        for (other_table_name, other_table) in &after.models {
            for col in &other_table.columns {
                let Some(fk) = col.foreign_key.as_ref() else {
                    continue;
                };
                if fk.ref_table != parent_table {
                    continue;
                }
                let constraint_name = format!("{}_{}_fkey", other_table_name, col.name);
                if other_table_name == &parent_table {
                    // Self-FK pair.
                    self_fk_cols.push(col.name.clone());
                    self_fk_constraints.push(constraint_name);
                    self_fk_deferrable.push(fk.deferrable);
                    self_fk_initially_deferred.push(fk.initially_deferred);
                    continue;
                }
                // Detect cycle: does the parent reference back at
                // `other_table_name` via one of its own FK columns?
                let cycle_back = if let Some(parent_schema) = after.models.get(&parent_table) {
                    parent_schema.columns.iter().find_map(|pc| {
                        let pfk = pc.foreign_key.as_ref()?;
                        if pfk.ref_table == *other_table_name {
                            Some(pc.name.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };
                if let Some(self_fk_col) = cycle_back {
                    cycles.push(PkFlipCycle {
                        peer_table: other_table_name.clone(),
                        peer_fk_column: col.name.clone(),
                        self_fk_column: self_fk_col,
                    });
                    // B-13 (Codex round-3): cycle peers are FIRST-CLASS
                    // children. The peer's FK column needs the same
                    // shadow-column orchestration as any other child
                    // (preparation, backfill, concurrent index, NOT
                    // NULL proof, cutover finalisation) — without it
                    // the cutover SQL references a `_desc` column
                    // that was never created. The `cycle_flag = true`
                    // marker tells the segment-3b FK emitter to attach
                    // `DEFERRABLE INITIALLY DEFERRED` to the
                    // constraint, and the cutover-level
                    // `SET CONSTRAINTS ALL DEFERRED` (gated on
                    // `!cycles.is_empty()`) tolerates mid-transaction
                    // FK states until COMMIT. Together these reproduce
                    // the playbook §8 deferred-constraints pattern.
                    children.push(PkFlipChild {
                        table: other_table_name.clone(),
                        fk_column: col.name.clone(),
                        fk_constraint_name: constraint_name,
                        on_delete: fk.on_delete,
                        // Codex round-4 B-16: cycle peers force
                        // deferrable + initially_deferred regardless
                        // of descriptor-side knobs. The cutover
                        // emits `SET CONSTRAINTS ALL DEFERRED` once
                        // at the top, but the recreated FK on the
                        // post-cutover column must ALSO be marked
                        // `DEFERRABLE INITIALLY DEFERRED` so future
                        // operator-driven deferred-FK use still
                        // works post-flip. Without this the cutover
                        // silently downgrades the cycle to
                        // non-deferrable; mid-tx FK violations on
                        // post-flip workloads then trip
                        // unconditionally even though the operator
                        // declared the cycle as deferrable.
                        fk_deferrable: true,
                        fk_initially_deferred: true,
                        fk_nullable: col.nullable,
                        fk_unique: col.unique,
                        family,
                        cycle_flag: true,
                    });
                    continue;
                }
                // Detect join-table: through-table whose two FKs
                // both reference parents in the migrating set OR a
                // through-table whose other FK references a peer
                // outside the migrating set.
                if other_table.is_through {
                    // Find the partner FK column (the OTHER FK on
                    // this through-table that does NOT point at
                    // this parent).
                    let partner = other_table.columns.iter().find_map(|pc| {
                        if pc.name == col.name {
                            return None;
                        }
                        let pfk = pc.foreign_key.as_ref()?;
                        if pfk.ref_table == parent_table {
                            return None;
                        }
                        Some((pc.name.clone(), pfk.ref_table.clone()))
                    });
                    let (partner_col, partner_constraint, partner_table) = match partner {
                        Some((pcol, partner_target))
                            if migrating_parents.contains(&partner_target) =>
                        {
                            // The other parent is also migrating —
                            // join-table participates in both
                            // migrations. Recorded with `Some(...)`.
                            let pcons = format!("{}_{}_fkey", other_table_name, pcol);
                            (Some(pcol), Some(pcons), Some(partner_target))
                        }
                        _ => (None, None, None),
                    };
                    // Codex round-4 B-16: extract partner-side
                    // FK deferrability from the partner column
                    // (when present). Without this lookup the
                    // cutover can't preserve a deferrable partner
                    // FK across the recreate boundary.
                    let (partner_def, partner_init_def) =
                        if let Some(partner_col_name) = partner_col.as_deref() {
                            other_table
                                .columns
                                .iter()
                                .find(|c| c.name == partner_col_name)
                                .and_then(|c| c.foreign_key.as_ref())
                                .map(|fk| (fk.deferrable, fk.initially_deferred))
                                .unwrap_or((false, false))
                        } else {
                            (false, false)
                        };
                    join_tables.push(PkFlipJoinTable {
                        table: other_table_name.clone(),
                        fk_to_parent_column: col.name.clone(),
                        fk_to_parent_constraint: constraint_name,
                        fk_to_parent_deferrable: fk.deferrable,
                        fk_to_parent_initially_deferred: fk.initially_deferred,
                        fk_to_partner_column: partner_col,
                        fk_to_partner_constraint: partner_constraint,
                        fk_to_partner_table: partner_table,
                        fk_to_partner_deferrable: partner_def,
                        fk_to_partner_initially_deferred: partner_init_def,
                        family,
                    });
                    continue;
                }
                // Ordinary child.
                children.push(PkFlipChild {
                    table: other_table_name.clone(),
                    fk_column: col.name.clone(),
                    fk_constraint_name: constraint_name,
                    on_delete: fk.on_delete,
                    // Codex round-4 B-16: preserve descriptor-
                    // declared deferrability through the cutover.
                    fk_deferrable: fk.deferrable,
                    fk_initially_deferred: fk.initially_deferred,
                    fk_nullable: col.nullable,
                    fk_unique: col.unique,
                    family,
                    cycle_flag: false,
                });
            }
        }

        // B-4r transitive FK closure (real, non-placeholder).
        //
        // Walk the FK graph rooted at `parent_table` via BFS so the
        // visited-set grows to include indirect descendants. The
        // closure is bounded by `MAX_CLOSURE_DEPTH` and protected
        // against cycles by the visited-set itself.
        //
        // **What `visited_tables` records.** Every table reachable
        // from `parent_table` via at least one FK that points at a
        // PK column of an already-visited table. For asc↔desc the
        // direct children list (`children`) is the only set whose
        // membership requires shadow-column orchestration — a
        // grandchild's FK column points at a CHILD's `id`, and the
        // child's `id` is itself a HeerId whose value space does NOT
        // re-key when the parent flips. The closure exists to:
        //
        //   1. Detect cycles defensively (a real-world cycle of
        //      arbitrary length cannot infinite-loop the differ).
        //   2. Terminate cleanly on a pathological graph
        //      (`MAX_CLOSURE_DEPTH = 65`).
        //   3. Reserve the structural plumbing for a future PK-type
        //      flip variant that DOES re-key grandchildren (e.g. a
        //      hypothetical TEXT key change). Such a variant would
        //      promote `visited_tables` membership to shadow-column
        //      orchestration by extending this body — no rewrite of
        //      the closure shape needed.
        //
        // **Cascade-depth panic.** At depth > 65 the closure panics
        // with the chain of tables that drove the depth blow-out so
        // tests can detect the contract violation deterministically.
        let mut visited_tables: BTreeSet<String> = BTreeSet::new();
        visited_tables.insert(parent_table.clone());
        for c in &children {
            visited_tables.insert(c.table.clone());
        }
        for jt in &join_tables {
            visited_tables.insert(jt.table.clone());
        }

        // Bound the closure depth defensively. A real-world FK graph
        // is typically <5 levels; 65 gives us headroom while
        // guaranteeing termination on a malformed graph.
        const MAX_CLOSURE_DEPTH: u32 = 65;
        // Chain-of-tables tracker so a depth blow-out reports the
        // sequence that drove it (operator-actionable signal).
        let mut depth_chain: Vec<String> = vec![parent_table.clone()];
        // Frontier for BFS — the set of tables whose children we have
        // not yet enumerated. Seeded with everything in the visited
        // set so the first pass scans direct children of the parent
        // AND of every direct-child table (depth-2). The first pass
        // therefore picks up grandchildren; subsequent passes pick up
        // great-grandchildren; etc.
        let mut frontier: BTreeSet<String> = visited_tables.clone();

        for depth in 1u32..=MAX_CLOSURE_DEPTH {
            if frontier.is_empty() {
                break;
            }
            // Scan every table in `after.models` for FKs whose
            // `ref_table` is a member of the current frontier. New
            // hits become next-pass frontier.
            let mut next_frontier: BTreeSet<String> = BTreeSet::new();
            for (other_name, other_schema) in &after.models {
                if visited_tables.contains(other_name) {
                    continue;
                }
                for col in &other_schema.columns {
                    let Some(fk) = col.foreign_key.as_ref() else {
                        continue;
                    };
                    if frontier.contains(&fk.ref_table) {
                        next_frontier.insert(other_name.clone());
                        break;
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            // Record one representative table from this depth so a
            // chain blow-out renders an operator-readable trail.
            if let Some(first) = next_frontier.iter().next() {
                depth_chain.push(first.clone());
            }
            for t in &next_frontier {
                visited_tables.insert(t.clone());
            }
            frontier = next_frontier;
            // B-4r (Codex round-3): the depth-65 contract returns
            // a structured error rather than panicking. The
            // operator-facing message renders the chain so the
            // offending FK shape is identifiable; compose / build
            // surface the error verbatim instead of unwinding.
            if depth == MAX_CLOSURE_DEPTH {
                return Err(DiffError::PkFlipCascadeDepthExceeded {
                    parent_table: parent_table.clone(),
                    chain: depth_chain.clone(),
                    max_depth: MAX_CLOSURE_DEPTH,
                });
            }
        }

        // Determinism: sort each collection.
        children.sort_by(|a, b| a.table.cmp(&b.table).then(a.fk_column.cmp(&b.fk_column)));
        join_tables.sort_by(|a, b| {
            a.table
                .cmp(&b.table)
                .then(a.fk_to_parent_column.cmp(&b.fk_to_parent_column))
        });
        cycles.sort_by(|a, b| a.peer_table.cmp(&b.peer_table));
        // Self-FK: pair the cols/constraints/deferrability flags in
        // alphabetical order. Codex round-4 B-16 widens the zipped
        // tuple to carry the per-FK deferrability — `(col, cons,
        // deferrable, initially_deferred)`. The cycle path forces
        // both flags to `true` further down, after this sort, so we
        // stay within the "data first, semantics second" ordering
        // the rest of the differ uses.
        let mut self_fk_zipped: Vec<(String, String, bool, bool)> = self_fk_cols
            .into_iter()
            .zip(self_fk_constraints)
            .zip(self_fk_deferrable)
            .zip(self_fk_initially_deferred)
            .map(|(((c, n), d), id)| (c, n, d, id))
            .collect();
        self_fk_zipped.sort_by(|a, b| a.0.cmp(&b.0));
        let self_fk = if self_fk_zipped.is_empty() {
            None
        } else {
            let mut cols: Vec<String> = Vec::with_capacity(self_fk_zipped.len());
            let mut cons: Vec<String> = Vec::with_capacity(self_fk_zipped.len());
            let mut deferr: Vec<bool> = Vec::with_capacity(self_fk_zipped.len());
            let mut init_def: Vec<bool> = Vec::with_capacity(self_fk_zipped.len());
            for (c, n, d, id) in self_fk_zipped {
                cols.push(c);
                cons.push(n);
                deferr.push(d);
                init_def.push(id);
            }
            // Cycle path forces deferrable + initially_deferred on
            // every self-FK when the parent participates in any
            // mutual-FK cycle. Same rationale as the cycle children
            // above — the cutover's `SET CONSTRAINTS ALL DEFERRED`
            // signals the runner-side discipline; the recreated FK
            // must carry the deferrable property post-cutover so
            // future operator-driven deferred-FK use still works.
            if !cycles.is_empty() {
                for d in deferr.iter_mut() {
                    *d = true;
                }
                for d in init_def.iter_mut() {
                    *d = true;
                }
            }
            Some(PkFlipSelfFk {
                fk_columns: cols,
                fk_constraint_names: cons,
                fk_deferrable: deferr,
                fk_initially_deferred: init_def,
            })
        };

        // Partitioned-parent metadata — pulled from the post-flip
        // schema where the partition declaration lives.
        let partitioned_parent = after
            .models
            .get(&parent_table)
            .and_then(|t| t.partition.as_ref())
            .map(|p| PkFlipPartitionedMeta {
                partition: p.clone(),
            });

        // Co-flag values: re-derive locally because this fn runs
        // before `classify()` would have computed them. Walk the
        // remaining ops and check.
        let co_destructive = ops.iter().any(|op| {
            matches!(
                op,
                SchemaOperation::DropTable(_)
                    | SchemaOperation::DropColumn { .. }
                    | SchemaOperation::DropEnum(_)
                    | SchemaOperation::DropIndex(_)
                    | SchemaOperation::DropForeignKey { .. }
            )
        });
        let co_lossy = ops.iter().any(|op| match op {
            SchemaOperation::AlterColumn { change, .. } => {
                matches!(change, ColumnChange::SetNullable(false))
            }
            SchemaOperation::AddColumn { column, .. } => {
                !column.nullable && column.default_sql.is_none()
            }
            _ => false,
        });

        ops.push(SchemaOperation::PkTypeFlipGroup(PkTypeFlipGroup {
            parent_table,
            parent_from,
            parent_to,
            direction,
            children,
            self_fk,
            join_tables,
            cycles,
            partitioned_parent,
            co_destructive,
            co_lossy,
            // Default join-table layout — Option A. The compose
            // pipeline overrides this from
            // `MigrateConfig::pk_flip_join_table_option` after the
            // differ runs, before the planner lowers the group.
            // See `apply_join_table_option` for the override entry
            // point.
            join_table_option: PkFlipJoinTableOption::OptionA,
        }));
    }
    Ok(())
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
        diff_pk_in_table(before_table, after_table, &column_renames, ops);
        diff_exclusion_constraints_in_table(before_table, after_table, ops);
        diff_table_metadata_in_table(before_table, after_table, ops);
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
        diff_pk_in_table(before_table, after_table, &column_renames, ops);
        diff_app_move_in_table(before_table, after_table, ops);
        diff_exclusion_constraints_in_table(before_table, after_table, ops);
        diff_table_metadata_in_table(before_table, after_table, ops);
    }
}

/// Detect changes to table-level DDL metadata between two snapshots of
/// the same table. Phase 8.5 Cluster 4 (djogi#172 umbrella).
///
/// Currently handles:
/// - `table_comment` → [`SchemaOperation::SetTableComment`] (djogi#217)
/// - `storage_params` → [`SchemaOperation::SetStorageParams`] (djogi#218)
/// - `tablespace` → [`SchemaOperation::SetTablespace`] (djogi#219)
///
/// Each slot emits one `SchemaOperation` when the before / after
/// values diverge; identical values produce no operation.
fn diff_table_metadata_in_table(
    before: &TableSchema,
    after: &TableSchema,
    ops: &mut Vec<SchemaOperation>,
) {
    // djogi#217 — table comment. Only emit when the value actually
    // changes so two consecutive differ runs against the same source
    // produce byte-identical output.
    if before.table_comment != after.table_comment {
        ops.push(SchemaOperation::SetTableComment {
            table: after.table.clone(),
            from: before.table_comment.clone(),
            to: after.table_comment.clone(),
        });
    }
    if before.storage_params != after.storage_params {
        ops.push(SchemaOperation::SetStorageParams {
            table: after.table.clone(),
            from: before.storage_params.clone(),
            to: after.storage_params.clone(),
        });
    }
    if before.tablespace != after.tablespace {
        ops.push(SchemaOperation::SetTablespace {
            table: after.table.clone(),
            from: before.tablespace.clone(),
            to: after.tablespace.clone(),
        });
    }
}

/// Detect added / dropped / modified `EXCLUDE` constraints between two
/// snapshots of the same table. Constraint identity is the `name`
/// field — modifying any other field (`using`, elements, where_clause,
/// deferrability) emits a drop+add pair so the live system always sees
/// a single canonical shape per name.
fn diff_exclusion_constraints_in_table(
    before: &TableSchema,
    after: &TableSchema,
    ops: &mut Vec<SchemaOperation>,
) {
    let before_by_name: BTreeMap<&str, &ExclusionConstraintSchema> = before
        .exclusion_constraints
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();
    let after_by_name: BTreeMap<&str, &ExclusionConstraintSchema> = after
        .exclusion_constraints
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    // Drops first so the down migration is symmetric with the up.
    for (name, before_excl) in &before_by_name {
        if after_by_name.contains_key(name) {
            continue;
        }
        ops.push(SchemaOperation::DropExclusionConstraint {
            table: after.table.clone(),
            name: (*name).to_string(),
            exclusion: (*before_excl).clone(),
        });
    }

    // Adds (including drop+add for modified constraints).
    for (name, after_excl) in &after_by_name {
        match before_by_name.get(name) {
            Some(before_excl) if before_excl == after_excl => {}
            Some(before_excl) => {
                ops.push(SchemaOperation::DropExclusionConstraint {
                    table: after.table.clone(),
                    name: (*name).to_string(),
                    exclusion: (*before_excl).clone(),
                });
                ops.push(SchemaOperation::AddExclusionConstraint {
                    table: after.table.clone(),
                    exclusion: (*after_excl).clone(),
                });
            }
            None => {
                ops.push(SchemaOperation::AddExclusionConstraint {
                    table: after.table.clone(),
                    exclusion: (*after_excl).clone(),
                });
            }
        }
    }
}

fn diff_columns_in_table<'a>(
    before: &TableSchema,
    after: &'a TableSchema,
    ops: &mut Vec<SchemaOperation>,
) -> BTreeMap<&'a str, &'a str> {
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

    // Return the rename map so the PK differ can normalise column
    // names before flip-pair detection — addresses Codex review's
    // "PK column rename + flip" edge case.
    column_rename_targets
}

fn emit_alter_column(
    parent: &TableSchema,
    before: &ColumnSchema,
    after: &ColumnSchema,
    ops: &mut Vec<SchemaOperation>,
) {
    let table = parent.table.clone();
    {
        let mut push = |change: ColumnChange| {
            ops.push(SchemaOperation::AlterColumn {
                table: table.clone(),
                column: after.name.clone(),
                change,
            });
        };
        let type_changed = before.sql_type != after.sql_type;
        if type_changed && before.check.is_some() {
            // If the old CHECK still references the pre-conversion type,
            // drop it before the type migration to avoid Postgres re-validating
            // it against the new type shape. The `from` carries the prior
            // expression so rollback can ADD it back after rolling the
            // type change back — without this, the down-side rollback
            // would leave the column un-checked (the bug GPT-5.5 review
            // flagged: lossy CHECK rollback on type-changed columns).
            push(ColumnChange::SetCheck {
                from: before.check.clone(),
                to: None,
            });
        }
        if type_changed {
            // djogi#220 — pull the adopter-supplied USING expression
            // from the AFTER column. The `type_change_using` slot is
            // `#[serde(skip)]` on `ColumnSchema`, so the BEFORE
            // (loaded-from-disk) side always carries `None`; only the
            // freshly-projected AFTER side ever carries `Some(...)`.
            // The expression is emitted verbatim into the migration's
            // `USING (<expr>)` clause; adopters own correctness.
            push(ColumnChange::ChangeType {
                from: before.sql_type.clone(),
                to: after.sql_type.clone(),
                using: after.type_change_using.clone(),
            });
        }
        if before.nullable != after.nullable {
            push(ColumnChange::SetNullable(after.nullable));
        }
        if before.default_sql != after.default_sql {
            push(ColumnChange::SetDefault(after.default_sql.clone()));
        }
        // CHECK constraint transitions — djogi#186 (Phase 8.5 v3 Cluster 2),
        // GPT-5.5 review (non-lossy rollback).
        //
        // Each emitted `SetCheck` carries both `from` (the CHECK on the
        // column at the start of this operation) and `to` (the CHECK
        // after this operation). The SQL emitter uses `from` for the
        // down-side, so rollback restores the prior CHECK rather than
        // leaving a comment-only placeholder.
        //
        // The AMEND case stays as a two-step DROP+ADD pair so each
        // step maps cleanly to one `OperationSql` (one up, one down).
        // Composing the down file in reverse order (per
        // `compose::compose_down_text`) gives the operator:
        //   step 2 down (DROP new), then step 1 down (ADD old) —
        // returning the column to its prior CHECK shape.
        //
        // For the type-change path, the post-type CHECK re-add carries
        // `from: None` because the prior `SetCheck { from: before.check,
        // to: None }` already dropped the original constraint at the
        // moment this entry runs. The composed down file rolls back
        // in reverse: drop the post-type CHECK, alter the type back,
        // re-add the original CHECK. Non-lossy.
        if type_changed && before.check.is_some() {
            if let Some(check) = &after.check {
                push(ColumnChange::SetCheck {
                    from: None,
                    to: Some(check.clone()),
                });
            }
        } else {
            match (&before.check, &after.check) {
                (None, None) => {}                 // unchanged
                (Some(b), Some(a)) if b == a => {} // unchanged
                (Some(b), Some(a)) => {
                    // AMEND — drop the old constraint, then add the new.
                    // Each step carries its full (from, to) so rollback
                    // is symmetric.
                    push(ColumnChange::SetCheck {
                        from: Some(b.clone()),
                        to: None,
                    });
                    push(ColumnChange::SetCheck {
                        from: None,
                        to: Some(a.clone()),
                    });
                }
                // ADD (None → Some) — one entry, lossless rollback via
                // DROP CONSTRAINT.
                (None, Some(a)) => {
                    push(ColumnChange::SetCheck {
                        from: None,
                        to: Some(a.clone()),
                    });
                }
                // DROP (Some → None) — one entry. `from` carries the
                // prior expression so rollback re-installs it via
                // ADD CONSTRAINT.
                (Some(b), None) => {
                    push(ColumnChange::SetCheck {
                        from: Some(b.clone()),
                        to: None,
                    });
                }
            }
        }
        if before.unique != after.unique {
            push(ColumnChange::SetUnique(after.unique));
        }
        if before.indexed != after.indexed {
            push(ColumnChange::SetIndexed(after.indexed));
        }
        if before.generated != after.generated {
            push(ColumnChange::SetGenerated {
                from: before.generated.clone(),
                to: after.generated.clone(),
            });
        }
        // Codex T22 BLOCK-3: detect IDENTITY transitions so old
        // snapshots (where `identity: None`) projected against new
        // schemas (where `identity: Some(ByDefault)` for Serial PKs)
        // emit the correct `ALTER COLUMN ADD GENERATED ... AS IDENTITY`
        // migration. Without this comparison the diff is silent and
        // adopters on existing tables don't get IDENTITY added,
        // leaving INSERT-without-id failing.
        if before.identity != after.identity {
            push(ColumnChange::SetIdentity {
                from: before.identity,
                to: after.identity,
            });
        }
        // Phase 8.5 djogi#217 — `#[field(comment)]` transitions
        // surface as a single `SetComment { from, to }` that the
        // emitter lowers to one `COMMENT ON COLUMN <t>.<c> IS …`
        // statement (or `IS NULL` when clearing). Only emit when the
        // value actually changes so two consecutive differ runs
        // produce byte-identical output.
        if before.comment != after.comment {
            push(ColumnChange::SetComment {
                from: before.comment.clone(),
                to: after.comment.clone(),
            });
        }
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
/// - custom PK shape changes (any transition involving
///   `PkKindSchema::Custom` on either side; see
///   [`custom_pk_unsupported_reason`] for the typed diagnostic shape)
///
/// **Custom PK transitions (djogi#165).** A model declared with a
/// `djogi::primary_key!` newtype on either side of the diff is rejected
/// with a dedicated message that names which side carries the custom
/// kind, the inner SQL types, and the type names. The stock
/// `pk_flip` family routes (`PkFlipFamily::Heer` / `Ranj`) only know
/// how to migrate the four built-in asc↔desc pairs — no playbook
/// exists for arbitrary `Custom → Custom`, `Custom → built-in`, or
/// `built-in → Custom` shape changes, and quietly emitting a generic
/// `ALTER COLUMN TYPE` would risk silent data loss when the inner SQL
/// types differ. See `docs/spec/migrations.md` §10.10a for the v0.1.0
/// support matrix.
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
    // djogi#165 — route any transition that involves a custom-PK
    // newtype on either side to its own diagnostic instead of the
    // generic debug-dump fallback. Rationale in the docstring above
    // and in `custom_pk_unsupported_reason`.
    if matches!(&before.primary_key.kind, PkKindSchema::Custom(_))
        || matches!(&after.primary_key.kind, PkKindSchema::Custom(_))
    {
        ops.push(SchemaOperation::Unsupported {
            reason: custom_pk_unsupported_reason(
                &after.table,
                &before.primary_key.kind,
                &after.primary_key.kind,
            ),
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

/// Format the v0.1.0 reject diagnostic for a primary-key transition
/// that involves a `djogi::primary_key!` custom newtype on at least
/// one side (djogi#165).
///
/// The message names the table, classifies the transition into one
/// of three buckets (`custom-to-custom`, `custom-to-built-in`,
/// `built-in-to-custom`), surfaces the full `type_name` / `sql_type` /
/// `default_sql` of the custom side(s) so a same-type but
/// different-`default_sql` change (e.g. a generator rotation) is
/// distinguishable from a same-generator type rename, and points
/// operators at the docs section that explains why `pk_flip.rs` does
/// not auto-migrate custom shapes in v0.1.0. Kept as a free fn so the
/// test mod can pin the exact phrasing per bucket.
fn custom_pk_unsupported_reason(
    table: &str,
    before: &PkKindSchema,
    after: &PkKindSchema,
) -> String {
    fn describe(kind: &PkKindSchema) -> String {
        match kind {
            PkKindSchema::Custom(c) => format!(
                "Custom(type_name = `{}`, sql_type = `{}`, default_sql = `{}`)",
                c.type_name, c.sql_type, c.default_sql,
            ),
            other => format!("{other:?}"),
        }
    }
    let before_desc = describe(before);
    let after_desc = describe(after);
    let bucket = match (before, after) {
        (PkKindSchema::Custom(_), PkKindSchema::Custom(_)) => "custom-to-custom",
        (PkKindSchema::Custom(_), _) => "custom-to-built-in",
        (_, PkKindSchema::Custom(_)) => "built-in-to-custom",
        // Defensive: the caller gates on at least one Custom side, but
        // this fallback keeps the format strictly total without
        // panicking if a future refactor relaxes the gate.
        _ => "non-custom",
    };
    format!(
        "table `{table}`: primary key change involves a \
         `djogi::primary_key!` custom newtype ({bucket}: {before_desc} → \
         {after_desc}) and is not auto-supported in v0.1.0. The \
         `pk_flip` family only ships migration playbooks for the four \
         built-in asc↔desc pairs (HeerId ↔ HeerIdRecencyBiased, \
         RanjId ↔ RanjIdRecencyBiased); transitions involving a custom \
         PK newtype must be hand-written so the operator can decide on \
         the value-preserving cast and the FK cascade strategy. See \
         `docs/spec/migrations.md` §10.10a for the v0.1.0 support \
         matrix and rationale."
    )
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Additive = 0,
    Reversible = 1,
    Destructive = 2,
    Lossy = 3,
}

fn severity_of(op: &SchemaOperation) -> Severity {
    match op {
        SchemaOperation::RenameTable { .. }
        | SchemaOperation::RenameColumn { .. }
        | SchemaOperation::RenameApp { .. }
        | SchemaOperation::MoveModelBetweenApps { .. } => Severity::Reversible,
        SchemaOperation::DropTable(_)
        | SchemaOperation::DropColumn { .. }
        | SchemaOperation::DropEnum(_)
        | SchemaOperation::DropIndex(_)
        | SchemaOperation::DropForeignKey { .. } => Severity::Destructive,
        SchemaOperation::AlterColumn { change, .. } => {
            if matches!(change, ColumnChange::SetNullable(false)) {
                Severity::Lossy
            } else {
                Severity::Additive
            }
        }
        SchemaOperation::AddColumn { column, .. } => {
            // A column is Lossy on add only if Postgres has no value
            // source for existing rows. Three sources are recognised:
            //   1. nullable column → NULL fills existing rows
            //   2. default_sql → DEFAULT expression fills existing rows
            //   3. generated → GENERATED ALWAYS expression fills rows
            // Only when none of these are set is the add Lossy.
            if !column.nullable && column.default_sql.is_none() && column.generated.is_none() {
                Severity::Lossy
            } else {
                Severity::Additive
            }
        }
        SchemaOperation::AddTable(_)
        | SchemaOperation::SetTableComment { .. }
        | SchemaOperation::SetStorageParams { .. }
        | SchemaOperation::SetTablespace { .. }
        | SchemaOperation::AddIndex(_)
        | SchemaOperation::AddEnum(_)
        | SchemaOperation::AddEnumVariant { .. }
        | SchemaOperation::AddForeignKey { .. }
        | SchemaOperation::AddExclusionConstraint { .. }
        | SchemaOperation::PkTypeFlip { .. }
        | SchemaOperation::PkTypeFlipGroup(_)
        | SchemaOperation::PkTypeFlipMultiGroup(_)
        | SchemaOperation::Unsupported { .. } => Severity::Additive,
        SchemaOperation::DropExclusionConstraint { .. } => Severity::Destructive,
    }
}

fn classify(ops: &[SchemaOperation]) -> Classification {
    if ops.is_empty() {
        return Classification::NoOp;
    }

    // Unsupported wins over everything: there's no apply plan.
    if let Some(reason) = ops.iter().find_map(|op| match op {
        SchemaOperation::Unsupported { reason } => Some(reason.clone()),
        _ => None,
    }) {
        return Classification::Unsupported { reason };
    }

    let has_pk_flip = ops.iter().any(|op| {
        matches!(
            op,
            SchemaOperation::PkTypeFlip { .. }
                | SchemaOperation::PkTypeFlipGroup(_)
                | SchemaOperation::PkTypeFlipMultiGroup(_)
        )
    });

    // PkTypeFlip is orthogonal — surface co-flags so the gate logic
    // can still apply destructive / lossy semantics. Compute each
    // flag independently: a list with only Lossy ops must not also
    // claim co_destructive, and a list with only Destructive ops
    // must not claim co_lossy.
    if has_pk_flip {
        let mut co_destructive = false;
        let mut co_lossy = false;
        for op in ops {
            match severity_of(op) {
                Severity::Destructive => co_destructive = true,
                Severity::Lossy => co_lossy = true,
                Severity::Additive | Severity::Reversible => {}
            }
        }
        return Classification::PkTypeFlip {
            co_destructive,
            co_lossy,
        };
    }

    let max = ops
        .iter()
        .map(severity_of)
        .max()
        .unwrap_or(Severity::Additive);

    match max {
        Severity::Additive => Classification::Additive,
        Severity::Reversible => Classification::Reversible,
        Severity::Destructive => Classification::Destructive,
        Severity::Lossy => Classification::Lossy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppDescriptor;
    use crate::descriptor::{
        EnumDescriptor, FieldDescriptor, FieldSqlType, IndexColumnSpec, IndexKind, IndexSpec,
        IndexTarget, IndexType, ModelDescriptor, PkType, field_descriptor, model_descriptor,
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
            ..model_descriptor(type_name, table, PkType::HeerIdDesc, &[])
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

    // ── djogi#165 — custom-PK newtype shape flips ─────────────────
    //
    // Every transition involving a `djogi::primary_key!`-declared
    // custom newtype on either side gets the typed reject diagnostic
    // from `custom_pk_unsupported_reason`. We pin the bucket label
    // and the type-name surfacing so operators see WHICH custom kind
    // changed, not a generic `PrimaryKeySchema { ... }` debug dump.
    //
    // The four cases below cover the matrix the issue calls out:
    //   1. same inner SQL type, different Rust newtype name
    //   2. same Rust newtype name, different inner SQL type
    //   3. built-in → custom transition
    //   4. custom → built-in transition
    //
    // The tests build descriptors directly via `PkType::Custom(...)`
    // — the same `CustomPrimaryKeyKind` shape the `primary_key!` macro
    // emits via inventory at adopter-build time — so we exercise the
    // exact projection / diff path the runtime takes without needing
    // the macro fixture to live under `djogi/src/`.

    fn synth_model_with_pk(
        table: &'static str,
        type_name: &'static str,
        pk: PkType,
    ) -> ModelDescriptor {
        ModelDescriptor {
            pk_type: pk,
            ..synth_model(table, type_name)
        }
    }

    /// Build before/after `users` descriptors with `before_pk` and
    /// `after_pk`, run them through the differ, and return the
    /// `Classification::Unsupported` reason string. Panics if the
    /// differ produced any other classification — the four reject
    /// tests below all assume `Unsupported` is the only correct
    /// answer for a custom-PK shape change.
    fn unsupported_pk_reason(before_pk: PkType, after_pk: PkType) -> String {
        let before = project_one(&synth_model_with_pk("users", "User", before_pk));
        let after = project_one(&synth_model_with_pk("users", "User", after_pk));
        let delta = diff_schemas(&before, &after, empty_global());
        match delta.classification {
            Classification::Unsupported { reason } => reason,
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    const CUSTOM_USER_ID: PkType = PkType::Custom(crate::descriptor::CustomPrimaryKeyKind {
        type_name: "crate::ids::UserId",
        sql_type: "BIGINT",
        default_sql: "user_id_next()",
    });
    const CUSTOM_USER_ID_V2: PkType = PkType::Custom(crate::descriptor::CustomPrimaryKeyKind {
        type_name: "crate::ids::UserIdV2",
        sql_type: "BIGINT",
        default_sql: "user_id_next()",
    });
    const CUSTOM_USER_ID_UUID: PkType = PkType::Custom(crate::descriptor::CustomPrimaryKeyKind {
        type_name: "crate::ids::UserId",
        sql_type: "UUID",
        default_sql: "gen_random_uuid()",
    });
    // Same `type_name` and `sql_type` as `CUSTOM_USER_ID` but a different
    // `default_sql` — pins the diagnostic surfaces every part of the
    // `CustomPkKindSchema` identity so operators can see what changed
    // when the generator function rotates (e.g. shard split, new
    // sequence cadence).
    const CUSTOM_USER_ID_NEXT_V2: PkType =
        PkType::Custom(crate::descriptor::CustomPrimaryKeyKind {
            type_name: "crate::ids::UserId",
            sql_type: "BIGINT",
            default_sql: "user_id_next_v2()",
        });

    #[test]
    fn pk_custom_same_inner_type_different_newtype_is_unsupported() {
        // Same inner SQL type (BIGINT) but the adopter renamed the
        // Rust newtype from `UserId` to `UserIdV2`. The `type_name`
        // field of `CustomPrimaryKeyKind` differs, so PartialEq says
        // "not equal" and the differ rejects with the custom-PK
        // diagnostic. The reject is intentional even though the
        // underlying column type is unchanged: a different Rust
        // newtype implies a different `PrimaryKey` impl, a different
        // `IntoFilterValue` discriminant, and potentially a different
        // `bulk_sql` / `default_sql` — the migration engine cannot
        // tell which fields the adopter changed under the rename.
        let reason = unsupported_pk_reason(CUSTOM_USER_ID, CUSTOM_USER_ID_V2);
        assert!(
            reason.contains("custom-to-custom"),
            "diagnostic must label the bucket; got: {reason}"
        );
        assert!(
            reason.contains("crate::ids::UserId") && reason.contains("crate::ids::UserIdV2"),
            "diagnostic must surface both type_names; got: {reason}"
        );
        assert!(
            reason.contains("djogi::primary_key!"),
            "diagnostic must mention the macro so operators know where to look; got: {reason}"
        );
    }

    #[test]
    fn pk_custom_changed_inner_sql_type_is_unsupported() {
        // Same `type_name` but the inner SQL type changes from BIGINT
        // to UUID. This is the dangerous case the issue calls out:
        // silently emitting an `ALTER COLUMN TYPE` would either fail
        // outright (no implicit BIGINT→UUID cast) or, worse, succeed
        // with a USING clause the operator never reviewed and
        // truncate live row IDs.
        let reason = unsupported_pk_reason(CUSTOM_USER_ID, CUSTOM_USER_ID_UUID);
        assert!(
            reason.contains("custom-to-custom"),
            "diagnostic must label the bucket; got: {reason}"
        );
        assert!(
            reason.contains("BIGINT") && reason.contains("UUID"),
            "diagnostic must surface both inner SQL types; got: {reason}"
        );
    }

    #[test]
    fn pk_custom_changed_default_sql_is_unsupported() {
        // Same `type_name` AND same `sql_type` — only the `default_sql`
        // changes. `CustomPkKindSchema` derives `PartialEq` over all
        // three identity fields, so a generator rotation (sequence
        // bump, shard split, new ID minting service) still fails the
        // equality check and routes through the custom-PK reject.
        //
        // The diagnostic MUST include both `default_sql` strings — without
        // them the operator would see two identical-looking `Custom(...)`
        // arms in the surfaced `ComposeError::UnsupportedDelta` and have
        // no way to tell what the differ actually objected to.
        let reason = unsupported_pk_reason(CUSTOM_USER_ID, CUSTOM_USER_ID_NEXT_V2);
        assert!(
            reason.contains("custom-to-custom"),
            "diagnostic must label the bucket; got: {reason}"
        );
        assert!(
            reason.contains("user_id_next()") && reason.contains("user_id_next_v2()"),
            "diagnostic must surface both default_sql generators so the \
             operator can see what changed; got: {reason}"
        );
    }

    #[test]
    fn pk_builtin_to_custom_is_unsupported() {
        // Migrating a model from the default HeerId to a custom
        // newtype lands here. No playbook exists for this transition
        // — both the column DEFAULT generator (heerid_next() →
        // user_id_next()) and the FK cascade strategy depend on the
        // adopter's intent, so we reject and let them hand-write it.
        let reason = unsupported_pk_reason(PkType::HeerId, CUSTOM_USER_ID);
        assert!(
            reason.contains("built-in-to-custom"),
            "diagnostic must label the bucket; got: {reason}"
        );
        assert!(
            reason.contains("crate::ids::UserId"),
            "diagnostic must surface the custom side's type_name; got: {reason}"
        );
        assert!(
            reason.contains("HeerId"),
            "diagnostic must surface the built-in side; got: {reason}"
        );
    }

    #[test]
    fn pk_custom_to_builtin_is_unsupported() {
        // Reverse direction — a model walks back from a custom
        // newtype to a built-in. Same reasoning: the value-preserving
        // cast is adopter-decided, not framework-derivable.
        let reason = unsupported_pk_reason(CUSTOM_USER_ID, PkType::HeerId);
        assert!(
            reason.contains("custom-to-built-in"),
            "diagnostic must label the bucket; got: {reason}"
        );
        assert!(
            reason.contains("crate::ids::UserId") && reason.contains("HeerId"),
            "diagnostic must surface both sides; got: {reason}"
        );
    }

    #[test]
    fn pk_custom_unchanged_is_noop() {
        // Sanity pin — when the custom PK shape is identical on both
        // sides (same type_name + sql_type + default_sql), the differ
        // must produce a NoOp. Without this, a stable custom-PK model
        // would emit a migration on every `compose` run.
        let m = synth_model_with_pk("users", "User", CUSTOM_USER_ID);
        let s = project_one(&m);
        let delta = diff_schemas(&s, &s, empty_global());
        assert_eq!(delta.classification, Classification::NoOp);
        assert!(delta.operations.is_empty());
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
        let deltas = diff_bucket_maps(&before, &after).expect("differ must succeed in this test");
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].bucket, empty_global());
        assert!(matches!(deltas[0].classification, Classification::Additive));
    }

    #[test]
    fn renamed_column_emits_rename_not_drop_add() {
        const OLD_NAME: FieldDescriptor = field_descriptor("old_name", FieldSqlType::Text, true);
        static OLD_SLICE: &[FieldDescriptor] = &[OLD_NAME];
        const NEW_NAME: FieldDescriptor = FieldDescriptor {
            renamed_from: Some("old_name"),
            ..field_descriptor("new_name", FieldSqlType::Text, true)
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

    // ── djogi#186 — CHECK constraint AMEND-replace lifecycle ───────────────
    //
    // The differ at this site (line ~2128) detects CHECK transitions
    // and emits ColumnChange::SetCheck variants. The AMEND case
    // (`Some(old) → Some(new)` with `old != new`) was originally lowered
    // as a single `ADD CONSTRAINT` against the existing constraint name,
    // which Postgres rejects because the name already exists from the
    // prior projection. The fix emits two `SetCheck` entries — first
    // `{ from: Some(old), to: None }` (drop) followed by
    // `{ from: None, to: Some(new) }` (add) — so the SQL pair is
    // `DROP CONSTRAINT` then `ADD CONSTRAINT`. GPT-5.5 review extended
    // the variant to carry `from` so each step's rollback restores the
    // pre-step state symmetrically. These tests pin all four cells of
    // the CHECK transition matrix so a future refactor cannot silently
    // regress any direction.

    fn build_table_with_check(check: Option<&str>) -> crate::migrate::schema::AppliedSchema {
        build_table_with_check_and_type(check, "BIGINT")
    }

    fn build_table_with_check_and_type(
        check: Option<&str>,
        sql_type: &str,
    ) -> crate::migrate::schema::AppliedSchema {
        use crate::migrate::schema::{
            AppliedSchema, ColumnSchema, PkKindSchema, PrimaryKeySchema, TableSchema,
        };
        use std::collections::BTreeMap;

        let id_col = ColumnSchema {
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
        };
        let amount_col = ColumnSchema {
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
            type_change_using: None,
        };
        let mut models = BTreeMap::new();
        models.insert(
            "widgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![id_col, amount_col],
                exclusion_constraints: vec![],
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
                table: "widgets".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            },
        );
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: "1".to_string(),
            generated_at: "2026-05-10T00:00:00Z".to_string(),
            indexes: vec![],
            models,
            registered_apps: vec!["".to_string()],
        }
    }

    fn alter_column_changes_for(
        delta: &crate::migrate::diff::SchemaDelta,
        column: &str,
    ) -> Vec<ColumnChange> {
        delta
            .operations
            .iter()
            .filter_map(|op| match op {
                SchemaOperation::AlterColumn {
                    column: c, change, ..
                } if c == column => Some(change.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn check_unchanged_some_emits_no_set_check() {
        let before = build_table_with_check(Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"));
        let after = build_table_with_check(Some("\"amount\" >= 0 AND \"amount\" <= 4294967295"));
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert!(
            changes.is_empty(),
            "identical CHECK on both sides must not emit any AlterColumn change: {changes:?}",
        );
    }

    #[test]
    fn check_unchanged_none_emits_no_set_check() {
        let before = build_table_with_check(None);
        let after = build_table_with_check(None);
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert!(
            changes.is_empty(),
            "absent CHECK on both sides must not emit any AlterColumn change: {changes:?}",
        );
    }

    #[test]
    fn check_add_emits_single_set_check_some() {
        // ADD scenario — descriptor evolves from i64 (no CHECK) to a
        // type that projects a CHECK. The differ emits a single
        // SetCheck { from: None, to: Some(new) } which the emitter
        // renders as `ALTER TABLE … ADD CONSTRAINT … CHECK (…)`.
        let after_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let before = build_table_with_check(None);
        let after = build_table_with_check(Some(after_expr));
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert_eq!(
            changes.len(),
            1,
            "ADD CHECK is a single ColumnChange entry: {changes:?}"
        );
        assert!(
            matches!(
                changes.first(),
                Some(ColumnChange::SetCheck { from: None, to: Some(s) }) if s == after_expr
            ),
            "ADD CHECK emits SetCheck {{ from: None, to: Some(new) }}: {changes:?}"
        );
    }

    #[test]
    fn table_metadata_changes_emit_dedicated_operations() {
        let mut before = build_table_with_check(None);
        let mut after = build_table_with_check(None);
        let before_table = before.models.get_mut("widgets").expect("before table");
        before_table.table_comment = Some("old comment".to_string());
        before_table.storage_params = Some("fillfactor=80".to_string());
        before_table.tablespace = Some("slowspace".to_string());
        let after_table = after.models.get_mut("widgets").expect("after table");
        after_table.table_comment = Some("new comment".to_string());
        after_table.storage_params = Some("fillfactor=70, autovacuum_enabled=false".to_string());
        after_table.tablespace = Some("fastspace".to_string());

        let delta = diff_schemas(&before, &after, empty_global());

        assert!(delta.operations.iter().any(|op| matches!(
            op,
            SchemaOperation::SetTableComment {
                table,
                from: Some(from),
                to: Some(to),
            } if table == "widgets" && from == "old comment" && to == "new comment"
        )));
        assert!(delta.operations.iter().any(|op| matches!(
            op,
            SchemaOperation::SetStorageParams {
                table,
                from: Some(from),
                to: Some(to),
            } if table == "widgets"
                && from == "fillfactor=80"
                && to == "fillfactor=70, autovacuum_enabled=false"
        )));
        assert!(delta.operations.iter().any(|op| matches!(
            op,
            SchemaOperation::SetTablespace {
                table,
                from: Some(from),
                to: Some(to),
            } if table == "widgets" && from == "slowspace" && to == "fastspace"
        )));
    }

    #[test]
    fn check_drop_emits_single_set_check_none() {
        // DROP scenario — descriptor evolves from u32 (with CHECK) to
        // i64 (no CHECK). The differ emits a single
        // SetCheck { from: Some(prior), to: None } which the emitter
        // renders as `ALTER TABLE … DROP CONSTRAINT …` and a
        // recoverable down-side that restores `prior`.
        let prior_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let before = build_table_with_check(Some(prior_expr));
        let after = build_table_with_check(None);
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert_eq!(
            changes.len(),
            1,
            "DROP CHECK is a single ColumnChange entry: {changes:?}"
        );
        assert!(
            matches!(
                changes.first(),
                Some(ColumnChange::SetCheck { from: Some(s), to: None }) if s == prior_expr
            ),
            "DROP CHECK emits SetCheck {{ from: Some(prior), to: None }}: {changes:?}"
        );
    }

    #[test]
    fn check_amend_emits_drop_then_add() {
        // AMEND scenario — the central djogi#186 lifecycle case.
        // Descriptor evolves from u32 → u64 (or any other CHECK
        // expression change). The differ MUST emit two ColumnChange
        // entries in order:
        //   SetCheck { from: Some(b), to: None }
        //   SetCheck { from: None, to: Some(a) }
        // The SQL emitter renders these as `DROP CONSTRAINT …; ADD
        // CONSTRAINT … CHECK (…);` — two separate ALTERs against the
        // same constraint name slot, which Postgres accepts cleanly.
        // Each step now carries its own (from, to) so the down side
        // restores the previous state symmetrically (GPT-5.5 fix).
        let before_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let after_expr = "\"amount\" >= 0 AND \"amount\" <= 18446744073709551615";
        let before = build_table_with_check(Some(before_expr));
        let after = build_table_with_check(Some(after_expr));
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert_eq!(
            changes.len(),
            2,
            "AMEND CHECK emits exactly two ColumnChange entries: {changes:?}"
        );
        assert!(
            matches!(
                changes.first(),
                Some(ColumnChange::SetCheck { from: Some(b), to: None }) if b == before_expr
            ),
            "AMEND step 1: SetCheck {{ from: Some(prior), to: None }}: {changes:?}"
        );
        assert!(
            matches!(
                changes.get(1),
                Some(ColumnChange::SetCheck { from: None, to: Some(a) }) if a == after_expr
            ),
            "AMEND step 2: SetCheck {{ from: None, to: Some(new) }}: {changes:?}"
        );
    }

    #[test]
    fn check_type_change_readds_same_check_after_type_change() {
        // When converting SQL type, an unchanged CHECK still needs a
        // drop+re-add around `ALTER COLUMN TYPE`, otherwise Postgres
        // can re-validate the old expression first and fail. The
        // first step's `from` carries the prior expression so the
        // down-side rollback restores it after reverting the type.
        let check_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let before = build_table_with_check_and_type(Some(check_expr), "INTEGER");
        let after = build_table_with_check_and_type(Some(check_expr), "BIGINT");
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert_eq!(
            changes.len(),
            3,
            "unchanged CHECK plus type migration must emit drop/type/readd steps: {changes:?}"
        );
        assert!(
            matches!(
                changes.first(),
                Some(ColumnChange::SetCheck { from: Some(b), to: None }) if b == check_expr
            ),
            "existing CHECK must be dropped before ALTER TYPE \
             (carrying `from` for rollback): {changes:?}"
        );
        assert!(
            matches!(changes.get(1), Some(ColumnChange::ChangeType { from, to, .. }) if from == "INTEGER" && to == "BIGINT"),
            "ALTER TYPE should be between drop and re-add: {changes:?}"
        );
        assert!(
            matches!(
                changes.get(2),
                Some(ColumnChange::SetCheck { from: None, to: Some(s) }) if s == check_expr
            ),
            "same CHECK should be re-added after type conversion \
             (forward-only `to`; the prior is restored by step 0's down): \
             {changes:?}"
        );
    }

    #[test]
    fn check_type_change_reorders_drop_and_readd_for_changed_check() {
        // Type migration with a CHECK expression change must still drop the
        // pre-migration CHECK before TYPE and re-add the post-migration
        // CHECK after it. The drop step carries the OLD expression in
        // `from`; the readd step carries the NEW expression in `to`.
        // Rollback walks down in reverse: drop new, revert type, ADD old.
        let before_expr = "\"amount\" >= 0 AND \"amount\" <= 4294967295";
        let after_expr = "\"amount\" >= 0 AND \"amount\" <= 18446744073709551615";
        let before = build_table_with_check_and_type(Some(before_expr), "INTEGER");
        let after = build_table_with_check_and_type(Some(after_expr), "BIGINT");
        let delta = diff_schemas(&before, &after, empty_global());
        let changes = alter_column_changes_for(&delta, "amount");
        assert_eq!(
            changes.len(),
            3,
            "changed CHECK + type migration must emit drop/type/readd steps: {changes:?}"
        );
        assert!(
            matches!(
                changes.first(),
                Some(ColumnChange::SetCheck { from: Some(b), to: None }) if b == before_expr
            ),
            "existing CHECK must be dropped before ALTER TYPE \
             with `from` carrying the OLD expression for rollback: {changes:?}"
        );
        assert!(
            matches!(changes.get(1), Some(ColumnChange::ChangeType { from, to, .. }) if from == "INTEGER" && to == "BIGINT"),
            "ALTER TYPE should be between drop and re-add: {changes:?}"
        );
        assert!(
            matches!(
                changes.get(2),
                Some(ColumnChange::SetCheck { from: None, to: Some(s) }) if s == after_expr
            ),
            "new CHECK should be re-added after type conversion \
             (the OLD expression is restored by step 0's down side): {changes:?}"
        );
    }

    // ── T22 round-3 BLOCK-2 / GAP-3 — SetIdentity diff regression ──

    #[test]
    fn diff_detects_identity_none_to_some_by_default_as_set_identity() {
        // Codex T22 round-3 GAP-3: a snapshot predating the
        // `identity` field (which deserialises as `None` via
        // `#[serde(default)]`) projected against a fresh schema
        // (Serial PKs project `identity = Some(ByDefault)`) must
        // surface a SetIdentity AlterColumn, not silently no-op.
        // Without this regression test, a sibling-site bug where
        // the identity comparison is omitted would slip past CI.
        use crate::migrate::schema::{
            AppliedSchema, ColumnSchema, IdentityKindSchema, PkKindSchema, PrimaryKeySchema,
            TableSchema,
        };
        use std::collections::BTreeMap;

        fn build_schema(identity: Option<IdentityKindSchema>) -> AppliedSchema {
            let id_col = ColumnSchema {
                check: None,
                comment: None,
                default_sql: None,
                foreign_key: None,
                generated: None,
                identity,
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
                sql_type: "INTEGER".to_string(),
                unique: false,
                type_change_using: None,
            };
            let table = TableSchema {
                app: None,
                columns: vec![id_col],
                exclusion_constraints: Vec::new(),
                fts: None,
                is_through: false,
                moved_from_app: None,
                partition: None,
                primary_key: PrimaryKeySchema {
                    columns: vec!["id".to_string()],
                    kind: PkKindSchema::Serial,
                },
                rationale: None,
                renamed_from: None,
                rls_enabled: false,
                table: "countries".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            };
            let mut models = BTreeMap::new();
            models.insert("countries".to_string(), table);
            AppliedSchema {
                djogi_version: "0.1.0".to_string(),
                enums: BTreeMap::new(),
                format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-05-02T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models,
                registered_apps: vec!["".to_string()],
            }
        }

        let before = build_schema(None);
        let after = build_schema(Some(IdentityKindSchema::ByDefault));
        let delta = diff_schemas(&before, &after, empty_global());

        let has_set_identity = delta.operations.iter().any(|op| {
            matches!(
                op,
                SchemaOperation::AlterColumn {
                    change: ColumnChange::SetIdentity {
                        from: None,
                        to: Some(IdentityKindSchema::ByDefault)
                    },
                    ..
                }
            )
        });
        assert!(
            has_set_identity,
            "expected SetIdentity {{ from: None, to: Some(ByDefault) }} in delta; got: {:?}",
            delta.operations
        );
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
    fn pk_flip_with_lossy_only_op_does_not_claim_co_destructive() {
        // Regression: classify() previously computed
        //     co_destructive: max >= Severity::Destructive
        // which is true whenever max == Lossy because Lossy > Destructive
        // in the severity ordering. That conflated the two flags so a
        // delta carrying ONLY a Lossy op (no Destructive op) wrongly
        // surfaced co_destructive=true and tripped the
        // --allow-destructive gate.
        //
        // The fix scans ops independently: co_destructive only when
        // some op classifies as Destructive, co_lossy only when some op
        // classifies as Lossy. This test pins both flags to their
        // expected values for a PK-flip + AddColumn(NOT NULL, no
        // default) delta.
        const REQUIRED: FieldDescriptor = field_descriptor("required", FieldSqlType::Text, false);
        static FIELDS: &[FieldDescriptor] = &[REQUIRED];
        let asc = ModelDescriptor {
            pk_type: PkType::HeerId,
            ..synth_model("widgets", "Widget")
        };
        let desc_with_required = ModelDescriptor {
            pk_type: PkType::HeerIdDesc,
            fields: FIELDS,
            ..synth_model("widgets", "Widget")
        };
        let before = project_one(&asc);
        let after = project_one(&desc_with_required);
        let delta = diff_schemas(&before, &after, empty_global());
        match delta.classification {
            Classification::PkTypeFlip {
                co_destructive,
                co_lossy,
            } => {
                assert!(
                    !co_destructive,
                    "Lossy-only op alongside PkTypeFlip must NOT set co_destructive"
                );
                assert!(
                    co_lossy,
                    "AddColumn(NOT NULL, no default) alongside PkTypeFlip must set co_lossy"
                );
            }
            other => panic!("expected PkTypeFlip with co_lossy only, got {other:?}"),
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

        let deltas = diff_bucket_maps(&before, &after).expect("differ must succeed in this test");

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
                    comment: None,
                    default_sql: None,
                    foreign_key: None,
                    generated: None,
                    identity: None,
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
                    type_change_using: None,
                }],
                exclusion_constraints: Vec::new(),
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
                table_comment: None,
                storage_params: None,
                tablespace: None,
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

    // ── B-4r: transitive FK closure unit tests ────────────────────────────

    /// Build a minimal `TableSchema` with the given name, a HeerId
    /// PK column called `id`, and the supplied FK columns.
    ///
    /// Each `(col_name, ref_table)` entry becomes a `BIGINT NOT NULL`
    /// column with an FK pointing at `ref_table.id`.
    fn synth_table_with_fks(
        table: &str,
        fks: &[(&str, &str)],
        pk_kind: crate::migrate::schema::PkKindSchema,
    ) -> crate::migrate::schema::TableSchema {
        use crate::migrate::schema::{
            ColumnSchema, ForeignKeySchema, OnDeleteSchema, PrimaryKeySchema, TableSchema,
        };
        let mut columns = Vec::new();
        // PK column
        columns.push(ColumnSchema {
            check: None,
            comment: None,
            default_sql: None,
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
        });
        for (col, ref_table) in fks {
            columns.push(ColumnSchema {
                check: None,
                comment: None,
                default_sql: None,
                foreign_key: Some(ForeignKeySchema {
                    deferrable: false,
                    initially_deferred: false,
                    on_delete: OnDeleteSchema::Restrict,
                    ref_column: "id".to_string(),
                    ref_table: ref_table.to_string(),
                }),
                generated: None,
                identity: None,
                index_type: None,
                indexed: false,
                max_length: None,
                name: col.to_string(),
                nullable: false,
                on_delete: Some(OnDeleteSchema::Restrict),
                outbox_exclude: false,
                rationale: None,
                relation_kind: None,
                renamed_from: None,
                sequence_within: None,
                sql_type: "BIGINT".to_string(),
                unique: false,
                type_change_using: None,
            });
        }
        TableSchema {
            app: None,
            columns,
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: pk_kind,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: table.to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    fn synth_schema_with_tables(
        tables: Vec<crate::migrate::schema::TableSchema>,
    ) -> crate::migrate::schema::AppliedSchema {
        let mut models = BTreeMap::new();
        for t in tables {
            models.insert(t.table.clone(), t);
        }
        crate::migrate::schema::AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models,
            registered_apps: vec!["".to_string()],
        }
    }

    fn synth_partitioned_cross_flipping_schema(
        left_pk: crate::migrate::schema::PkKindSchema,
        right_pk: crate::migrate::schema::PkKindSchema,
    ) -> crate::migrate::schema::AppliedSchema {
        use crate::migrate::schema::{
            ColumnSchema, ForeignKeySchema, OnDeleteSchema, PartitionSchema, PrimaryKeySchema,
            TableSchema,
        };

        let left = TableSchema {
            app: None,
            columns: vec![
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
                },
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
                    name: "ts".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "TIMESTAMPTZ".to_string(),
                    unique: false,
                    type_change_using: None,
                },
            ],
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: Some(PartitionSchema::Range {
                column: "ts".to_string(),
            }),
            primary_key: PrimaryKeySchema {
                columns: vec!["ts".to_string(), "id".to_string()],
                kind: left_pk,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "left_events".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        };
        let right = synth_table_with_fks("right_tags", &[], right_pk);
        let join = TableSchema {
            app: None,
            columns: vec![
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
                },
                ColumnSchema {
                    check: None,
                    comment: None,
                    default_sql: None,
                    foreign_key: Some(ForeignKeySchema {
                        deferrable: false,
                        initially_deferred: false,
                        on_delete: OnDeleteSchema::Restrict,
                        ref_column: "id".to_string(),
                        ref_table: "left_events".to_string(),
                    }),
                    generated: None,
                    identity: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "left_event_id".to_string(),
                    nullable: false,
                    on_delete: Some(OnDeleteSchema::Restrict),
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                    type_change_using: None,
                },
                ColumnSchema {
                    check: None,
                    comment: None,
                    default_sql: None,
                    foreign_key: Some(ForeignKeySchema {
                        deferrable: false,
                        initially_deferred: false,
                        on_delete: OnDeleteSchema::Restrict,
                        ref_column: "id".to_string(),
                        ref_table: "right_tags".to_string(),
                    }),
                    generated: None,
                    identity: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "right_tag_id".to_string(),
                    nullable: false,
                    on_delete: Some(OnDeleteSchema::Restrict),
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                    type_change_using: None,
                },
            ],
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: true,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["left_event_id".to_string(), "right_tag_id".to_string()],
                kind: crate::migrate::schema::PkKindSchema::Serial,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "event_tags".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        };
        synth_schema_with_tables(vec![left, right, join])
    }

    /// 4-level cascade A → B → C → D where A flips. The closure
    /// should walk through every level (visited_tables grows to
    /// include B, C, D) without panicking. For asc↔desc the
    /// children list contains B as a direct child (depth 1) — C
    /// and D become reachable via the closure but are NOT
    /// shadow-column targets for this flip variant.
    #[test]
    fn transitive_fk_closure_walks_four_level_cascade() {
        use crate::migrate::schema::PkKindSchema;
        // A has the flipping PK; B → A, C → B, D → C.
        let after = synth_schema_with_tables(vec![
            synth_table_with_fks("a", &[], PkKindSchema::HeerId),
            synth_table_with_fks("b", &[("a_id", "a")], PkKindSchema::HeerId),
            synth_table_with_fks("c", &[("b_id", "b")], PkKindSchema::HeerId),
            synth_table_with_fks("d", &[("c_id", "c")], PkKindSchema::HeerId),
        ]);
        let mut ops = vec![SchemaOperation::PkTypeFlip {
            table: "a".to_string(),
            from: PkKindSchema::HeerId,
            to: PkKindSchema::HeerIdRecencyBiased,
        }];
        // Closure must terminate without panicking.
        promote_pk_flips_to_groups(&after, &mut ops).expect("closure must not error in this test");
        // Resulting group: B is a direct child of A; C and D are NOT
        // children for the asc↔desc flip (their FKs point at B's /
        // C's `id`, not A's). The transitive closure walked them but
        // did not promote them to shadow-column orchestration.
        let group = ops
            .iter()
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("PkTypeFlipGroup emitted");
        assert_eq!(group.parent_table, "a");
        assert_eq!(
            group
                .children
                .iter()
                .map(|c| c.table.as_str())
                .collect::<Vec<_>>(),
            vec!["b"],
            "for asc↔desc only depth-1 children become shadow-column targets",
        );
    }

    /// Cycle A ↔ B: A has FK to B, B has FK to A. Closure must
    /// terminate (visited-set protection) and the cycle peer is
    /// recorded BOTH as a [`PkFlipCycle`] (drives cutover-level
    /// `SET CONSTRAINTS ALL DEFERRED`) AND as a [`PkFlipChild`] with
    /// `cycle_flag = true` (drives shadow-column orchestration plus
    /// per-FK `DEFERRABLE INITIALLY DEFERRED`). B-13 (Codex round-3)
    /// promoted cycle peers from cycle-only metadata to first-class
    /// children so every segment emitter (preparation, backfill,
    /// concurrent index, NOT NULL proof, cutover) iterates them
    /// uniformly with the rest of the cascade.
    #[test]
    fn transitive_fk_closure_terminates_on_cycle() {
        use crate::migrate::schema::PkKindSchema;
        let after = synth_schema_with_tables(vec![
            // a.b_id references b
            synth_table_with_fks("a", &[("b_id", "b")], PkKindSchema::HeerId),
            // b.a_id references a — closes the cycle
            synth_table_with_fks("b", &[("a_id", "a")], PkKindSchema::HeerId),
        ]);
        let mut ops = vec![SchemaOperation::PkTypeFlip {
            table: "a".to_string(),
            from: PkKindSchema::HeerId,
            to: PkKindSchema::HeerIdRecencyBiased,
        }];
        // Must not loop indefinitely — visited-set + frontier
        // tracking guarantees termination.
        promote_pk_flips_to_groups(&after, &mut ops).expect("closure must not error in this test");
        let group = ops
            .iter()
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("PkTypeFlipGroup emitted");
        assert_eq!(group.cycles.len(), 1, "cycle peer detected");
        assert_eq!(group.cycles[0].peer_table, "b");
        // B-13: peer also lands in `children` with cycle_flag = true
        // so segments 1 / 2 / 3 / 3b / 4 / 5 emit the b.a_id_desc
        // shadow-column orchestration.
        let cycle_children: Vec<_> = group
            .children
            .iter()
            .filter(|c| c.cycle_flag)
            .map(|c| (c.table.as_str(), c.fk_column.as_str()))
            .collect();
        assert_eq!(cycle_children, vec![("b", "a_id")]);
    }

    /// 3-level cascade with no cycles — sanity: closure walks
    /// past depth-1 without spuriously panicking.
    #[test]
    fn transitive_fk_closure_handles_three_level_cascade_without_panic() {
        use crate::migrate::schema::PkKindSchema;
        let after = synth_schema_with_tables(vec![
            synth_table_with_fks("p", &[], PkKindSchema::HeerId),
            synth_table_with_fks("c1", &[("p_id", "p")], PkKindSchema::HeerId),
            synth_table_with_fks("g1", &[("c_id", "c1")], PkKindSchema::HeerId),
        ]);
        let mut ops = vec![SchemaOperation::PkTypeFlip {
            table: "p".to_string(),
            from: PkKindSchema::HeerId,
            to: PkKindSchema::HeerIdRecencyBiased,
        }];
        promote_pk_flips_to_groups(&after, &mut ops).expect("closure must not error in this test");
        let group = ops
            .iter()
            .find_map(|op| match op {
                SchemaOperation::PkTypeFlipGroup(g) => Some(g),
                _ => None,
            })
            .expect("group");
        // Depth-1 children only (asc↔desc invariant).
        assert_eq!(group.children.len(), 1);
        assert_eq!(group.children[0].table, "c1");
    }

    /// B-4r (Codex round-3): a chain longer than the closure's
    /// `MAX_CLOSURE_DEPTH` returns a structured
    /// [`DiffError::PkFlipCascadeDepthExceeded`] rather than
    /// panicking.
    ///
    /// **Codex round-4 B-4r PARTIAL — depth test routes through
    /// `diff_bucket_maps`.** Round-3's test invoked
    /// `promote_pk_flips_to_groups` directly. Codex round-4
    /// flagged this as a missed round-trip: the differ's
    /// `diff_bucket_maps` is the only call path production
    /// callers (compose / build / runner) ever take. Routing the
    /// test through that path proves the depth-65 contract holds
    /// when the differ owns op promotion (it does: the per-bucket
    /// finalisation step in `diff_bucket_maps` calls
    /// `promote_pk_flips_to_groups` with `?` propagation, so the
    /// `DiffError` surfaces unchanged).
    ///
    /// Test setup:
    ///   * `before`: 70-level FK chain (P → T1 → ... → T70) with
    ///     P's PK kind set to `HeerId`.
    ///   * `after`: SAME 70-level chain with P's PK kind flipped
    ///     to `HeerIdRecencyBiased`. The differ emits one
    ///     `PkTypeFlip` op for `p` and the closure walks the
    ///     chain.
    ///   * Assert `diff_bucket_maps` returns
    ///     `Err(DiffError::PkFlipCascadeDepthExceeded { ... })`
    ///     with the chain populated.
    #[test]
    fn diff_bucket_maps_emits_pk_flip_cascade_depth_exceeded_on_deep_graph() {
        use crate::migrate::projection::BucketKey;
        use crate::migrate::schema::PkKindSchema;
        // Helper: build the 70-level chain with parametric P PK
        // kind so before/after differ only on P's PK.
        let build_chain = |p_pk: PkKindSchema| -> AppliedSchema {
            let mut tables: Vec<crate::migrate::schema::TableSchema> = Vec::new();
            tables.push(synth_table_with_fks("p", &[], p_pk));
            for i in 1..=70u32 {
                let prev = if i == 1 {
                    "p".to_string()
                } else {
                    format!("t{}", i - 1)
                };
                let name = format!("t{i}");
                let prev_str = prev.as_str();
                let table =
                    synth_table_with_fks(&name, &[("ref_id", prev_str)], PkKindSchema::HeerId);
                tables.push(table);
            }
            synth_schema_with_tables(tables)
        };
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        };
        let before: BTreeMap<BucketKey, AppliedSchema> = {
            let mut m = BTreeMap::new();
            m.insert(bucket.clone(), build_chain(PkKindSchema::HeerId));
            m
        };
        let after: BTreeMap<BucketKey, AppliedSchema> = {
            let mut m = BTreeMap::new();
            m.insert(
                bucket.clone(),
                build_chain(PkKindSchema::HeerIdRecencyBiased),
            );
            m
        };
        let err = diff_bucket_maps(&before, &after)
            .expect_err("70-level chain must trigger depth contract via diff_bucket_maps");
        match err {
            DiffError::PkFlipCascadeDepthExceeded {
                parent_table,
                chain,
                max_depth,
            } => {
                assert_eq!(parent_table, "p");
                assert_eq!(max_depth, 65);
                // Chain must include the parent + at least one
                // entry per depth level; final length depends on
                // whether the BFS picked p itself as depth-0.
                assert!(
                    chain.len() >= 2,
                    "chain must record at least the parent and one descendant: {chain:?}",
                );
                assert_eq!(chain[0], "p");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn diff_bucket_maps_rejects_partitioned_cross_flipping_cluster() {
        use crate::migrate::projection::BucketKey;
        use crate::migrate::schema::PkKindSchema;

        let bucket = BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        };
        let before: BTreeMap<BucketKey, AppliedSchema> = {
            let mut m = BTreeMap::new();
            m.insert(
                bucket.clone(),
                synth_partitioned_cross_flipping_schema(PkKindSchema::HeerId, PkKindSchema::HeerId),
            );
            m
        };
        let after: BTreeMap<BucketKey, AppliedSchema> = {
            let mut m = BTreeMap::new();
            m.insert(
                bucket.clone(),
                synth_partitioned_cross_flipping_schema(
                    PkKindSchema::HeerIdRecencyBiased,
                    PkKindSchema::HeerIdRecencyBiased,
                ),
            );
            m
        };

        let err = diff_bucket_maps(&before, &after)
            .expect_err("partitioned + cross-flipping cluster must reject");
        match err {
            DiffError::PartitionedMultiParentClusterUnsupported {
                partitioned_parents,
                cross_flipping_partners,
            } => {
                assert_eq!(partitioned_parents, vec!["left_events".to_string()]);
                assert_eq!(
                    cross_flipping_partners,
                    vec!["left_events".to_string(), "right_tags".to_string()]
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Display rendering of `DiffError::PkFlipCascadeDepthExceeded`
    /// — the operator-facing message identifies the chain root,
    /// reports the contract limit, and prints the trail. Pure
    /// formatting test, no differ invocation needed.
    #[test]
    fn pk_flip_cascade_depth_exceeded_display_renders_operator_message() {
        let display = format!(
            "{}",
            DiffError::PkFlipCascadeDepthExceeded {
                parent_table: "p".to_string(),
                chain: vec!["p".to_string(), "t1".to_string()],
                max_depth: 65,
            }
        );
        assert!(display.contains("rooted at p"));
        assert!(display.contains("65 levels"));
        assert!(display.contains("table_chain"));
    }
}
