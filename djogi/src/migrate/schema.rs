//! Internal schema model — the in-memory representation that
//! `schema_snapshot.json` round-trips through and that the Phase 7
//! differ (T2) compares against.
//!
//! The descriptor types in [`crate::descriptor`] use `&'static`
//! slices everywhere because they are populated at compile time via
//! `inventory::submit!`. Snapshot types are owned (`String`,
//! `Vec<...>`, `BTreeMap<String, ...>`) because they round-trip
//! through `serde_json` and need to survive load-from-disk. The two
//! representations are intentionally distinct: the projection from
//! [`crate::descriptor::ModelDescriptor`] to [`AppliedSchema`] is
//! the single boundary that translates static-lifetime constants into
//! owned data, and any future descriptor field that should land in
//! the snapshot extends both shapes deliberately.
//!
//! # Determinism
//!
//! Serialization is deterministic so that `git diff schema_snapshot.json`
//! is reviewable. Determinism rules:
//!
//! - Maps use `BTreeMap` (alphabetical key order on serialize).
//! - Vectors are sorted by stable identity (table name, index name,
//!   app label) at projection time, before any serde call.
//! - Struct fields are declared in alphabetical order so serde
//!   emits them alphabetically.
//! - Enum variants and their inner shapes are stable — adding a new
//!   variant is a snapshot-format change requiring a `format_version`
//!   bump.
//!
//! # `format_version` policy
//!
//! Bump only on breaking shape changes; additive fields do not bump.
//! The current value is `"1"`. The loader rejects any other value
//! with a clear upgrade message — see
//! [`crate::migrate::snapshot::load_snapshot`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The current snapshot format version.
///
/// Loaders compare against this exact string. A snapshot whose
/// `format_version` is not equal to this value is rejected with an
/// error directing the operator to upgrade Djogi or check out an
/// older revision.
///
/// **Bump policy.** Every snapshot struct carries
/// `#[serde(deny_unknown_fields)]`, so any structural change — adding
/// a field, removing a field, renaming a field — requires a
/// `format_version` bump because old loaders no longer accept the
/// new shape and new loaders no longer accept the old shape. There
/// is no "additive only" silent path; the loader fails loudly when
/// the shape drifts. A future version migration would land via a
/// dedicated phase with a parallel-read compatibility window.
pub const SNAPSHOT_FORMAT_VERSION: &str = "1";

/// Top-level snapshot — the committed source of truth for what the
/// schema looks like as of the last successful `djogi migrations
/// apply` for a given `(target, app)` pair.
///
/// Stored on disk at `migrations/<target>/<app>/schema_snapshot.json`.
/// One file per `(target, app)`; the synthetic global bucket lives
/// at `<target>/<empty-string-label>/`.
///
/// The fields are declared alphabetically so serde emits them in
/// alphabetical order — see the module-level "Determinism" docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedSchema {
    /// Djogi version that wrote this snapshot. Informational only —
    /// the loader does not gate on it. Useful for forensics when a
    /// snapshot looks wrong.
    pub djogi_version: String,

    /// Enums declared via `#[derive(DjogiEnum)]` and registered
    /// through `inventory::submit!`. Keyed by the enum's Postgres
    /// type name (the `#[djogi_enum(sql_name = "...")]` value).
    pub enums: BTreeMap<String, EnumSchema>,

    /// The snapshot format version. Currently always `"1"`. See
    /// [`SNAPSHOT_FORMAT_VERSION`].
    pub format_version: String,

    /// Snapshot generation timestamp, RFC 3339, UTC, second
    /// precision. Informational only.
    pub generated_at: String,

    /// Indexes — flat list, sorted by `(table, name)` for
    /// determinism. Each entry carries its `table` so the differ
    /// can group by table without a separate index registry.
    pub indexes: Vec<IndexSchema>,

    /// Models in this `(target, app)` bucket, keyed by Postgres
    /// table name. `BTreeMap` for alphabetical-key serialization.
    pub models: BTreeMap<String, TableSchema>,

    /// App labels that were registered when this snapshot was
    /// generated. Synthetic global bucket (no `#[model(app = ...)]`)
    /// is represented by the empty string `""`. Sorted
    /// alphabetically. Used by the `build.rs` D004 (folder drift)
    /// diagnostic and by `verify` to detect filesystem-vs-snapshot
    /// drift.
    pub registered_apps: Vec<String>,
}

/// Per-table snapshot. Mirrors the runtime [`crate::descriptor::
/// ModelDescriptor`], translated to owned data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableSchema {
    /// App label this table belongs to. `None` for the synthetic
    /// global bucket, `Some(label)` for `#[model(app = SomeApp)]`.
    pub app: Option<String>,

    /// Columns in declaration order — including the framework-injected
    /// `id`, `created_at`, `updated_at`. Order is preserved (not
    /// sorted) because Postgres `CREATE TABLE` cares about column
    /// order and the snapshot must round-trip exactly.
    pub columns: Vec<ColumnSchema>,

    /// Full-text search configuration when the model carries
    /// `#[model(fts = { source = "...", dictionary = "..." })]`.
    /// Snapshot is the single place the differ reads to detect
    /// dictionary changes (which require a generated-column rebuild
    /// — see [`crate::fts::FtsDescriptor`] notes).
    pub fts: Option<FtsSchema>,

    /// `true` when the table is a `#[model(table = "...", through)]`
    /// junction table for a specific `impl ManyToMany<Target> for Source`.
    pub is_through: bool,

    /// Model's prior app, when `#[model(moved_from_app = OldApp)]`
    /// is set. Drives the differ's "move model between apps" delta.
    pub moved_from_app: Option<String>,

    /// Partition strategy when `#[model(partition_by = "...")]` is
    /// set. `None` for non-partitioned tables (the common case).
    pub partition: Option<PartitionSchema>,

    /// Primary-key shape — drives the column DEFAULT, the bulk-
    /// allocation primitive used during seeding, and (for `Custom`)
    /// the adopter's wire-up. See
    /// [`crate::primary_key::PrimaryKey`].
    pub primary_key: PrimaryKeySchema,

    /// Human-readable rationale for non-obvious model design choices,
    /// surfaced in `djogi docs` and in the migration file header.
    pub rationale: Option<String>,

    /// `#[model(renamed_from = "old_table")]` value. Drives the
    /// differ's table-rename detection so the operation emits as
    /// `ALTER TABLE ... RENAME TO ...` rather than DROP+CREATE.
    pub renamed_from: Option<String>,

    /// `true` when the table has a `#[model(tenant_key = "col")]`
    /// declared and Phase 5 RLS policy generation should fire. The
    /// column itself is one of the entries in `columns`; this flag
    /// is the differ's signal that the policy DDL accompanies the
    /// table DDL.
    pub rls_enabled: bool,

    /// Postgres table name. Redundant with the `models` map key
    /// but stored explicitly so a `TableSchema` value is
    /// self-contained (e.g. when iterating `applied.models.values()`).
    pub table: String,

    /// `#[model(tenant_key = "col_name")]` value. `Some(col)`
    /// activates RLS policy generation against that column.
    pub tenant_key: Option<String>,
}

/// Per-column snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnSchema {
    /// Optional `CHECK (...)` constraint expression — raw SQL,
    /// emitted verbatim. The differ compares by string equality.
    /// `None` for the common case.
    pub check: Option<String>,

    /// `DEFAULT` expression — raw SQL. Empty `None` denotes no
    /// default. For PK columns with a server-generated default
    /// (`generate_id()`, `gen_random_uuid()`, ...), this is set
    /// from the descriptor's PK kind via the projection.
    pub default_sql: Option<String>,

    /// Foreign key declaration when this column stores the source
    /// side of an FK / O2O relation. `None` for scalar columns.
    pub foreign_key: Option<ForeignKeySchema>,

    /// Column-level `INDEX USING <method>` override — `None` falls
    /// back to BTree at emission time.
    pub index_type: Option<IndexTypeSchema>,

    /// `true` when the column has an implicit `#[field(index)]`
    /// declaration. Composite / non-default indexes live in the
    /// top-level `indexes` slice instead.
    pub indexed: bool,

    /// Maximum length for `VARCHAR(N)` / similar types. `None`
    /// means unlimited or not applicable.
    pub max_length: Option<u32>,

    /// Postgres column name.
    pub name: String,

    /// `NOT NULL` enforcement — `false` here means the column is
    /// nullable.
    pub nullable: bool,

    /// Cascade discipline when the column carries an FK relation.
    /// Always `Some(_)` when `foreign_key.is_some()`. The
    /// projection fills the `Restrict` default when the descriptor
    /// declares no explicit cascade.
    pub on_delete: Option<OnDeleteSchema>,

    /// `#[field(outbox = "ignore")]` — exclude from outbox payload.
    /// Captured in the snapshot because the differ may want to
    /// observe outbox-exclusion changes (informational only — no
    /// SQL impact).
    pub outbox_exclude: bool,

    /// `#[field(rationale = "...")]` value. Surfaced in `djogi
    /// docs` and migration headers.
    pub rationale: Option<String>,

    /// Relation cardinality when the column carries an FK / O2O.
    /// `None` for scalar columns.
    pub relation_kind: Option<RelationKindSchema>,

    /// `#[field(renamed_from = "old_name")]` — drives the differ's
    /// column-rename detection.
    pub renamed_from: Option<String>,

    /// Parent FK column scoping a monotonic per-parent sequence
    /// (`#[field(sequence_within = "parent_fk")]`). `None` outside
    /// the scope-sequencing pattern.
    pub sequence_within: Option<String>,

    /// Canonical Postgres type rendered as text — e.g. `"TEXT"`,
    /// `"BIGINT"`, `"geography(Point, 4326)"`. The runtime
    /// descriptor stores a typed [`crate::descriptor::FieldSqlType`];
    /// the snapshot stores the rendered form because the snapshot
    /// is the comparison surface for the differ — comparing rendered
    /// strings is robust against `Display` implementations evolving.
    pub sql_type: String,

    /// `UNIQUE` constraint at the column level. Composite uniqueness
    /// lives in the top-level `indexes` slice.
    pub unique: bool,
}

/// Foreign-key declaration on a column.
///
/// The `on_delete` field is the FK's authoritative cascade discipline.
/// [`ColumnSchema::on_delete`] mirrors it so adopters can still ask the
/// column what its cascade is without traversing the FK, but the
/// snapshot's source of truth for the cascade lives here — the
/// projection populates both fields from the same descriptor input,
/// and the differ / SQL emitter read this one when lowering FK ops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeySchema {
    /// Cascade discipline on delete of the referenced row. Mirrors
    /// the column-level [`ColumnSchema::on_delete`] but lives here so
    /// the FK-only operations ([`crate::migrate::diff::SchemaOperation::AddForeignKey`]
    /// / [`crate::migrate::diff::SchemaOperation::DropForeignKey`])
    /// can carry the cascade through to the SQL emitter without
    /// needing the full `ColumnSchema`. Codex T3 review B-3 fixed an
    /// earlier bug where the emitter unconditionally lowered `ON
    /// DELETE RESTRICT` regardless of the declared cascade.
    pub on_delete: OnDeleteSchema,

    /// Target column. Always `"id"` in current Djogi (FKs reference
    /// the parent's PK), but stored explicitly so future column-
    /// targeting FKs round-trip cleanly.
    pub ref_column: String,

    /// Target table name.
    pub ref_table: String,
}

/// `ON DELETE` cascade discipline. Mirrors
/// [`crate::relation::OnDelete`] in name, lifted to the snapshot
/// crate so the snapshot does not depend on relation-internal types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnDeleteSchema {
    /// Postgres default — block deletion if a referent exists.
    Restrict,
    /// Cascade the delete to dependent rows.
    Cascade,
    /// Null out the FK column on delete (column must be nullable).
    SetNull,
    /// Restore the column DEFAULT on delete.
    SetDefault,
    /// `NO ACTION` — defer the check, fail at constraint check time.
    NoAction,
}

/// Relation cardinality. Mirrors
/// [`crate::relation::RelationKind`] in name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationKindSchema {
    /// `ForeignKey<T>` / `Option<ForeignKey<T>>` — many-to-one.
    ForeignKey,
    /// `OneToOneField<T>` / `Option<OneToOneField<T>>` — one-to-one.
    OneToOne,
}

/// Postgres index method. Mirrors
/// [`crate::descriptor::IndexType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexTypeSchema {
    BTree,
    Gin,
    Gist,
    Hash,
    Spgist,
    Brin,
}

/// Per-column knobs inside an [`IndexTargetSchema::Columns`] list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexColumnSchema {
    /// Column name. Order in the parent `Vec` is significant — an
    /// index on `(a, b)` accelerates a different set of queries
    /// than one on `(b, a)`.
    pub name: String,

    /// `NULLS FIRST` / `NULLS LAST` / Postgres default.
    pub nulls: IndexNullsOrderSchema,

    /// Per-column opclass (`text_pattern_ops`, etc.). `None` lets
    /// Postgres pick the default for the column's data type.
    pub opclass: Option<String>,

    /// Per-column sort direction.
    pub order: IndexOrderSchema,
}

/// Sort direction inside an [`IndexColumnSchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexOrderSchema {
    Asc,
    Desc,
}

/// `NULLS FIRST` / `NULLS LAST` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexNullsOrderSchema {
    /// Postgres default — `NULLS LAST` for `Asc`, `NULLS FIRST`
    /// for `Desc`. The emitter omits the explicit clause for this
    /// variant.
    Default,
    First,
    Last,
}

/// Index target — column list or expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexTargetSchema {
    /// One or more columns with optional per-column opclass / order
    /// / nulls knobs.
    Columns(Vec<IndexColumnSchema>),
    /// Expression-form index — raw SQL stored verbatim.
    Expression(String),
}

/// Index uniqueness discipline. Mirrors
/// [`crate::descriptor::IndexKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKindSchema {
    /// Plain index — `CREATE INDEX`.
    NonUnique,
    /// `UNIQUE` constraint on the table — `ALTER TABLE ... ADD
    /// CONSTRAINT ... UNIQUE (...)`.
    UniqueConstraint,
    /// `CREATE UNIQUE INDEX` without a constraint row — required
    /// for partial uniqueness and `NULLS NOT DISTINCT`.
    UniqueIndex,
}

/// Index snapshot — fields declared alphabetically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSchema {
    /// Postgres extension required before the index can be created
    /// (e.g. `"postgis"`). `None` for stock BTree / GIN / ... indexes.
    pub extension_dependency: Option<String>,

    /// `INCLUDE(...)` payload columns. Empty when unused.
    pub include: Vec<String>,

    /// Postgres index method (BTree, GiST, etc.).
    pub index_type: IndexTypeSchema,

    /// Uniqueness discipline.
    pub kind: IndexKindSchema,

    /// Index name. Globally unique within the database. The differ
    /// keys index identity off this name.
    pub name: String,

    /// `NULLS NOT DISTINCT` modifier on `UNIQUE INDEX`. Forces
    /// `UniqueIndex` kind. Always `false` for non-unique indexes.
    pub nulls_not_distinct: bool,

    /// Partial-index `WHERE` clause — raw SQL, emitted verbatim.
    /// `None` for full-table indexes.
    pub predicate: Option<String>,

    /// `true` when the emitter must run this index DDL outside any
    /// transaction (e.g. `CREATE INDEX CONCURRENTLY`). Drives
    /// segment planning in T3.
    pub requires_out_of_transaction: bool,

    /// Owning table name.
    pub table: String,

    /// Column list or expression target.
    pub target: IndexTargetSchema,
}

/// Partition strategy for the parent table. Mirrors
/// [`crate::descriptor::PartitionSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionSchema {
    /// `PARTITION BY RANGE (column)` — typically time-series data.
    Range { column: String },
    /// `PARTITION BY HASH (column) PARTITIONS n`.
    Hash { column: String, partitions: u16 },
}

/// Full-text search configuration. Mirrors
/// [`crate::fts::FtsDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FtsSchema {
    /// The generated tsvector column name.
    pub column: String,
    /// Postgres dictionary name passed to `to_tsvector(...)`.
    /// **Changing this is a column rebuild — the differ must treat
    /// it as drop+add, not in-place ALTER.** See
    /// [`crate::fts::FtsDescriptor`].
    pub dictionary: String,
    /// Source column expression — typically the column being indexed.
    pub source: String,
}

/// Primary-key shape and identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryKeySchema {
    /// PK column names in order. For the default single-column
    /// `id` shape this is `vec!["id"]`. Composite PKs (rare,
    /// mostly join tables) carry multiple entries.
    pub columns: Vec<String>,

    /// PK strategy — drives the column DEFAULT and the bulk-
    /// allocation primitive.
    pub kind: PkKindSchema,
}

/// Concrete PK strategy. Mirrors [`crate::descriptor::PkType`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PkKindSchema {
    /// 64-bit time-ordered, ascending — `BIGINT DEFAULT generate_id()`.
    HeerId,
    /// 64-bit recency-biased — most-recent-first BTree scans without
    /// a secondary descending index. `BIGINT DEFAULT
    /// generate_id_desc()`.
    HeerIdRecencyBiased,
    /// 128-bit UUIDv8 ascending — `UUID DEFAULT generate_ranj_id()`.
    RanjId,
    /// 128-bit UUIDv8 recency-biased — `UUID DEFAULT
    /// generate_ranj_id_desc()`.
    RanjIdRecencyBiased,
    /// Postgres `IDENTITY` integer — for lookup / reference tables.
    Serial,
    /// Composite PK — N columns. Rare; mostly join tables.
    Composite,
    /// No PK — model declared `#[model(pk = None)]`. The framework
    /// still emits the framework columns but no `PRIMARY KEY`
    /// constraint.
    None,
    /// Adopter-declared custom PK via `djogi::primary_key! { ... }`.
    /// Carries the wire-up needed for DDL emission.
    Custom(CustomPkKindSchema),
}

/// Snapshot form of [`crate::descriptor::CustomPrimaryKeyKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomPkKindSchema {
    /// Column DEFAULT — empty string when client-generated (no
    /// server-side default).
    pub default_sql: String,
    /// Postgres column type — e.g. `"UUID"`, `"BIGINT"`.
    pub sql_type: String,
    /// Fully-qualified Rust type name, e.g. `"crate::ids::UserId"`.
    /// Drives the differ's "this is the same custom PK" check; a
    /// rename here forces a PK migration even when the SQL type
    /// is identical.
    pub type_name: String,
}

/// Enum snapshot. One entry per `#[derive(DjogiEnum)]` registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumSchema {
    /// Postgres `CREATE TYPE` name. Redundant with the `enums` map
    /// key for self-containment — same pattern as `TableSchema.table`.
    pub name: String,
    /// Variant labels in declaration order. Postgres `ALTER TYPE
    /// ADD VALUE` ordering is meaningful, so the snapshot preserves
    /// declaration order rather than sorting.
    pub variants: Vec<String>,
}

/// Forward-reference for Phase 7.5 — the migration runner will tag
/// segments with this enum so the live-plan layer can ladder its
/// orchestration on top of Phase 7's planner without restructuring
/// `migrate/`. Phase 7 itself does not switch on this; T3's segment
/// planner sets every segment to `Safe` for now.
///
/// Declaring the seam in this crate (rather than waiting for Phase
/// 7.5) prevents a later refactor from churning the `migrate/`
/// internals.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnlineSafety {
    /// Every operation in this segment is safe to run while the
    /// application is serving traffic. The default for Phase 7.
    Safe,
    /// At least one operation in this segment requires operator
    /// orchestration (compatibility window, staged backfill, etc.)
    /// — Phase 7.5's territory.
    RequiresLivePlan,
}
