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

    /// Table-level `EXCLUDE` constraints declared via
    /// `#[model(exclusion(...))]`. Empty for the common case. Sorted
    /// by `name` for determinism. `#[serde(default)]` so snapshots
    /// predating this field round-trip cleanly.
    #[serde(default)]
    pub exclusion_constraints: Vec<ExclusionConstraintSchema>,

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

    /// `GENERATED ALWAYS AS (<expr>) STORED` declaration. `None` for
    /// regular columns (the common case). Set when the field carries
    /// `#[field(generated = "<expr>")]`. `#[serde(default)]` so
    /// snapshots predating this field round-trip cleanly.
    #[serde(default)]
    pub generated: Option<GeneratedColumnSchema>,

    /// `GENERATED BY DEFAULT AS IDENTITY` / `GENERATED ALWAYS AS IDENTITY`
    /// declaration on the column. `None` for non-identity columns.
    /// Distinct from [`Self::generated`] which models computed columns
    /// (`GENERATED ALWAYS AS (<expr>) STORED`); this slot models
    /// auto-incrementing identity columns whose value comes from a
    /// sequence rather than an expression.
    ///
    /// Set by the projection for `pk = Serial` models (the id column
    /// gets `Some(IdentityKindSchema::ByDefault)`). FK columns
    /// referencing a Serial PK stay `None` — sequence ownership lives
    /// on the parent's PK column, not its references.
    ///
    /// `#[serde(default)]` so snapshots predating this field round-trip
    /// cleanly. Snapshots from before the IDENTITY-emitting fix (#86)
    /// still load; the differ emits the proper
    /// `ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY` migration
    /// when the projection now produces `Some(...)` against an old
    /// `None` snapshot.
    #[serde(default)]
    pub identity: Option<IdentityKindSchema>,

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
    ///
    /// **Substrate-mirror field — not consumed by the SQL emitter.**
    /// T3's SQL emitter reads [`ForeignKeySchema::on_delete`]
    /// (the conceptual home for cascade) for both inline and
    /// standalone FK paths. This field is retained on the column for
    /// future non-SQL consumers (e.g. `cargo djogi inspect`,
    /// descriptor-level diagnostics) and as a snapshot record of the
    /// per-column declaration. Removing it would be a substrate
    /// change beyond T3's charter; see Codex T3 review A-1 for the
    /// dual-source-of-truth rationale.
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
///
/// **Deferrability (Codex round-4 B-16).** `deferrable` /
/// `initially_deferred` reproduce Postgres' two-axis FK
/// deferrability model: a constraint can be `DEFERRABLE` or `NOT
/// DEFERRABLE`, and a `DEFERRABLE` constraint is either `INITIALLY
/// DEFERRED` (checks postponed to COMMIT unless explicitly
/// `SET CONSTRAINTS IMMEDIATE`) or `INITIALLY IMMEDIATE` (checks
/// run at every statement until explicitly `SET CONSTRAINTS
/// DEFERRED`). Both fields default to `false` for backward
/// compatibility with existing snapshots that predate the field
/// (the serde default keeps old snapshot files round-tripping
/// without manual migration). The PK-flip cutover preserves
/// these flags when re-creating FKs across the cutover boundary;
/// previously the cutover always emitted plain `ADD CONSTRAINT
/// FOREIGN KEY (...)`, silently downgrading deferrable FKs to
/// non-deferrable. Cycle FKs are an exception — the cycle path
/// FORCES `deferrable = true, initially_deferred = true`
/// regardless of descriptor input, because cycles structurally
/// require deferred-constraint semantics for the cutover
/// transaction body to commit (the same `SET CONSTRAINTS ALL
/// DEFERRED` discipline the playbook §8 calls out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeySchema {
    /// `true` iff the FK was declared `DEFERRABLE` at creation
    /// time. When `false`, `initially_deferred` MUST also be
    /// `false` — Postgres rejects `INITIALLY DEFERRED` on a
    /// non-deferrable constraint. The PK-flip cutover preserves
    /// the live FK's deferrability across the recreate boundary;
    /// see the type-level doc.
    #[serde(default)]
    pub deferrable: bool,

    /// `true` iff the FK is `INITIALLY DEFERRED`. Only meaningful
    /// when `deferrable = true` (Postgres rejects the combination
    /// otherwise). When `deferrable = true && initially_deferred =
    /// false` the FK is `DEFERRABLE INITIALLY IMMEDIATE` — checks
    /// run at every statement, but operators can opt into
    /// transaction-scoped deferral via `SET CONSTRAINTS x DEFERRED`.
    #[serde(default)]
    pub initially_deferred: bool,

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

/// `GENERATED ALWAYS AS (<expr>) STORED` declaration on a column.
/// Mirrors [`crate::descriptor::GeneratedColumnSpec`] in owned form.
///
/// Pg18 only supports `STORED`; `stored = false` is reserved for the
/// future Pg19+ `VIRTUAL` variant and is rejected by the macro today.
/// `GENERATED ... AS IDENTITY` flavor — identity-column declaration.
///
/// `ByDefault` means INSERTs that supply an explicit `id` are honored
/// (Postgres' typical lookup-table pattern). `Always` means INSERTs
/// that supply an explicit `id` are rejected unless the caller uses
/// `OVERRIDING SYSTEM VALUE`. Cluster E ships `ByDefault` for
/// `pk = Serial` since lookup-table seeding often supplies fixed IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityKindSchema {
    /// `GENERATED BY DEFAULT AS IDENTITY` — auto-increment when id is
    /// omitted; honored when explicitly supplied.
    ByDefault,
    /// `GENERATED ALWAYS AS IDENTITY` — auto-increment only; explicit
    /// id requires `OVERRIDING SYSTEM VALUE` at INSERT time.
    Always,
}

impl IdentityKindSchema {
    /// SQL keyword stub for inline emission inside a column definition
    /// or for `ALTER COLUMN ADD GENERATED ...` migrations.
    pub fn sql_clause(self) -> &'static str {
        match self {
            Self::ByDefault => "GENERATED BY DEFAULT AS IDENTITY",
            Self::Always => "GENERATED ALWAYS AS IDENTITY",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedColumnSchema {
    /// SQL expression evaluated to produce the column value. Emitted
    /// verbatim inside `GENERATED ALWAYS AS (<expression>) STORED`.
    pub expression: String,
    /// `true` emits `STORED`. The macro currently rejects `false` —
    /// the field exists so future Pg19+ `VIRTUAL` columns round-trip
    /// cleanly without a snapshot-format bump.
    pub stored: bool,
}

/// Per-column member of an [`ExclusionConstraintSchema`].
///
/// One entry per `<expr> WITH <op>` clause inside the `EXCLUDE`
/// constraint body. `EXCLUDE USING gist (room_id WITH =, period WITH &&)`
/// decomposes into two entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionElementSchema {
    /// Column name or expression. Emitted verbatim before `WITH`.
    pub expr: String,
    /// Postgres operator class member used for the exclusion comparison
    /// (e.g. `"="`, `"&&"`, `"<>"`). Emitted verbatim after `WITH`.
    pub with_operator: String,
}

/// Table-level `EXCLUDE` constraint declaration. Mirrors
/// [`crate::descriptor::ExclusionConstraintSpec`] in owned form.
///
/// Adding an `EXCLUDE` constraint to a populated table classifies as
/// [`OnlineSafetyClassification::OfflineOnly`] — Postgres 18 has no
/// `NOT VALID` for `EXCLUDE`, so two-phase staging is structurally
/// impossible. The empty-table case (CREATE TABLE inline, or an
/// existing table with zero rows) flows through the regular
/// `OnlineSafe` path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionConstraintSchema {
    /// `true` emits `DEFERRABLE`. The macro enforces that
    /// `initially_deferred = true` requires `deferrable = true`.
    #[serde(default)]
    pub deferrable: bool,
    /// Element list in declaration order. Order is preserved verbatim
    /// in the emitted DDL because `EXCLUDE` operator class semantics
    /// depend on element order.
    pub elements: Vec<ExclusionElementSchema>,
    /// `true` emits `INITIALLY DEFERRED`. Only meaningful when
    /// `deferrable = true`.
    #[serde(default)]
    pub initially_deferred: bool,
    /// Constraint name. Drives diff identity — two constraints with
    /// the same name on the same table are considered the same
    /// constraint by the differ.
    pub name: String,
    /// Index method (e.g. `"gist"`, `"btree"`). Emitted verbatim into
    /// `EXCLUDE USING <method>`.
    pub using: String,
    /// Optional `WHERE` predicate. Raw SQL, emitted verbatim. `None`
    /// means the constraint applies to every row.
    pub where_clause: Option<String>,
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

/// Online-safety classification for a migration segment — the
/// frozen Phase 7 ↔ Phase 7.5 boundary contract. Every column-level
/// or constraint-level migration operation that reaches the segment
/// planner is tagged with exactly one of the four variants below.
///
/// # Boundary contract (§6.5)
///
/// `OnlineSafetyClassification` and the
/// `SchemaOperation::PkTypeFlipGroup` /
/// `SchemaOperation::PkTypeFlipMultiGroup` cascade routes are
/// **mutually exclusive**. A primary-key type flip is orchestrated
/// natively by Phase 7's `migrate::pk_flip` emitter family — when a
/// delta carries a PK-flip group, the classifier short-circuits
/// because that operation is already routed through its dedicated
/// path. `OnlineSafetyClassification::ExpandContract` therefore never
/// overlaps with PK-flip work; PK flips sit architecturally below the
/// live-plan layer.
///
/// # Consumption boundary
///
/// Phase 7.5's `live_migrate` module consumes **only**
/// `OnlineSafetyClassification::ExpandContract` — the variant whose
/// handoff marker is the spec term `RequiresLivePlan`. The other
/// three variants stay inside Phase 7:
///
/// - `OnlineSafe` is applied directly by the runner.
/// - `FastLockDestructiveGuarded` is gated on `--allow-destructive`
///   and applied directly; no live plan is generated.
/// - `OfflineOnly` is refused outright; the operator must
///   acknowledge downtime or perform manual handling.
///
/// `OfflineOnly` and `FastLockDestructiveGuarded` are
/// operator-acknowledgement branches, **not** live-plan branches.
///
/// # Naming
///
/// This enum answers a different question than
/// [`crate::migrate::diff::Classification`]:
/// `OnlineSafetyClassification` tags a single migration *operation*
/// with its online-safety verdict (the four variants below), while
/// `diff::Classification` tags a whole *delta* with its severity /
/// routing (`NoOp` / `Additive` / `Reversible` / `Destructive` /
/// `Lossy` / `Unsupported{reason}` / `PkTypeFlip{...}`). The two live
/// at different granularities on `SchemaDelta` and the rename
/// guarantees that `use` lines and match arms cannot mix them up.
///
/// # Stability
///
/// Marked `#[non_exhaustive]` so future online-safety categories can
/// land without a breaking change. Downstream `match` against this
/// enum from outside the `djogi` crate must include a wildcard arm
/// (`_ => …`); exhaustive matches inside `djogi` continue to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnlineSafetyClassification {
    /// Pure additive change — no lock held longer than the Postgres
    /// fast-path window, no data loss, no replication-lag hazard.
    /// The runner applies it directly with no operator gate.
    OnlineSafe,
    /// Completes inside the Pg18 fast-path lock window but destroys
    /// data or invalidates dependents (DROP COLUMN, DROP INDEX of a
    /// referenced index, etc.). Phase 7 gates application behind
    /// `--allow-destructive`; no live plan is generated.
    FastLockDestructiveGuarded,
    /// Cannot complete safely in a single segment — Phase 7.5
    /// generates a live plan and the operator drives the
    /// expand → backfill → flip → contract sequence. The handoff
    /// marker for this variant is the spec term `RequiresLivePlan`,
    /// and `live_migrate` consumes only this variant.
    ExpandContract,
    /// Djogi refuses to emit SQL. The operator must explicitly
    /// acknowledge downtime or perform the change by hand — there is
    /// no online path for this delta.
    OfflineOnly,
}

#[cfg(test)]
mod online_safety_classification_tests {
    use super::OnlineSafetyClassification;

    #[test]
    fn online_safety_classification_has_four_distinct_variants() {
        let all = [
            OnlineSafetyClassification::OnlineSafe,
            OnlineSafetyClassification::FastLockDestructiveGuarded,
            OnlineSafetyClassification::ExpandContract,
            OnlineSafetyClassification::OfflineOnly,
        ];
        for (i, lhs) in all.iter().enumerate() {
            for (j, rhs) in all.iter().enumerate() {
                if i == j {
                    assert_eq!(lhs, rhs);
                } else {
                    assert_ne!(lhs, rhs, "variants at {i} and {j} must differ");
                }
            }
        }
    }

    /// Boundary contract (§6.5): only `ExpandContract` is the
    /// live-plan handoff variant. The other three are
    /// operator-acknowledgement / direct-apply branches that stay in
    /// Phase 7 — the live-plan layer must never accept them.
    #[test]
    fn only_expand_contract_routes_to_live_plan() {
        let routes_to_live_plan = |c: OnlineSafetyClassification| -> bool {
            matches!(c, OnlineSafetyClassification::ExpandContract)
        };
        assert!(routes_to_live_plan(
            OnlineSafetyClassification::ExpandContract
        ));
        assert!(!routes_to_live_plan(OnlineSafetyClassification::OnlineSafe));
        assert!(!routes_to_live_plan(
            OnlineSafetyClassification::FastLockDestructiveGuarded
        ));
        assert!(!routes_to_live_plan(
            OnlineSafetyClassification::OfflineOnly
        ));
    }
}
