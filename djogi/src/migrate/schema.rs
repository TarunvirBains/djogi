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
/// `#[serde(deny_unknown_fields)]`. That guard rejects **unknown**
/// fields on the loader side — it does **not** error on missing
/// ones. As a result, two distinct change classes have different
/// bump requirements:
///
/// 1. **Additive fields with `#[serde(default)]`** — do NOT bump.
///    Older snapshots written without the new field deserialize
///    against the new shape because the default supplies the value;
///    nothing the loader doesn't recognise appears on the wire.
///    Examples in this module: `TableSchema::exclusion_constraints`,
///    `ColumnSchema::generated`, `ColumnSchema::identity`,
///    `ForeignKeySchema::deferrable`,
///    `ForeignKeySchema::initially_deferred`. Phase 8.5 Cluster 4
///    (djogi#217 / #218 / #219) lands
///    `TableSchema::table_comment`, `TableSchema::storage_params`,
///    `TableSchema::tablespace`, and `ColumnSchema::comment` under
///    this same rule.
/// 2. **Renames, removals, and variant reshapes** — DO bump. Older
///    loaders accept different field names / shapes than the new
///    loader, and there is no defaulting that bridges the gap; the
///    `format_version` is the explicit incompatibility signal so a
///    parallel-read compatibility window can be planned.
///
/// A future version migration would land via a dedicated phase with
/// a parallel-read compatibility window.
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

    /// `#[model(storage_params = "key=val, ...")]` value when set —
    /// Phase 8.5 djogi#218. Lowered to `ALTER TABLE <t> SET
    /// (key=val, ...)` by the migration composer after table creation;
    /// the differ surfaces value changes via
    /// [`crate::migrate::diff::SchemaOperation::SetStorageParams`].
    #[serde(default)]
    pub storage_params: Option<String>,

    /// Postgres table name. Redundant with the `models` map key
    /// but stored explicitly so a `TableSchema` value is
    /// self-contained (e.g. when iterating `applied.models.values()`).
    pub table: String,

    /// `#[model(table_comment = "…")]` value when set — Phase 8.5
    /// djogi#217. Lowered to `COMMENT ON TABLE <t> IS '<text>'` by
    /// the migration composer after `CREATE TABLE`; the differ
    /// surfaces value changes via
    /// [`crate::migrate::diff::SchemaOperation::SetTableComment`].
    ///
    /// `#[serde(default)]` keeps snapshots predating this field
    /// round-tripping cleanly — older snapshots load as `None` and
    /// the differ then sees the projected `Some(…)` as a new comment
    /// to install. `None` is the common case.
    #[serde(default)]
    pub table_comment: Option<String>,

    /// `#[model(tablespace = "<name>")]` value when set — Phase 8.5
    /// djogi#219. Lowered to `ALTER TABLE <t> SET TABLESPACE <name>`.
    /// `None` means the database default tablespace.
    #[serde(default)]
    pub tablespace: Option<String>,

    /// `#[model(tenant_key = "col_name")]` value. `Some(col)`
    /// activates RLS policy generation against that column.
    pub tenant_key: Option<String>,
}

/// Per-column snapshot.
///
/// **PartialEq exclusion.** `PartialEq` / `Eq` are implemented manually
/// below to exclude the transient [`ColumnSchema::type_change_using`]
/// slot — see that field's doc for the full rationale. Every other
/// field participates in equality, so the manual impl must be updated
/// in lockstep whenever a new persistent field lands on `ColumnSchema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnSchema {
    /// Optional `CHECK (...)` constraint expression — raw SQL,
    /// emitted verbatim. The differ compares by string equality.
    /// `None` for the common case.
    pub check: Option<String>,

    /// `#[field(comment = "<text>")]` value when set — Phase 8.5
    /// djogi#217. Lowered to `COMMENT ON COLUMN <t>.<c> IS
    /// '<text>'` by the migration composer immediately after the
    /// column appears in either `CREATE TABLE` (initial creation)
    /// or `ADD COLUMN` (later addition); the differ surfaces value
    /// changes via
    /// [`crate::migrate::diff::ColumnChange::SetComment`].
    ///
    /// `#[serde(default)]` keeps snapshots predating this field
    /// round-tripping cleanly. `None` is the common case.
    #[serde(default)]
    pub comment: Option<String>,

    /// `DEFAULT` expression — raw SQL. Empty `None` denotes no
    /// default. For PK columns with a server-generated default
    /// (`heerid_next()`, `gen_random_uuid()`, ...), this is set
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

    /// Adopter-supplied `#[field(type_change_using = "<sql expr>")]`
    /// USING clause for non-default-cast column type changes — Phase 8.5
    /// Cluster 4 djogi#220.
    ///
    /// **Transit only.** This slot is populated by the projection from
    /// the descriptor, **read by the differ's column-walk in
    /// [`crate::migrate::diff`] (`emit_alter_column`) to populate
    /// `ColumnChange::ChangeType::using`** on the
    /// emitted operation, and never persisted to the on-disk snapshot
    /// — `#[serde(skip)]` excludes the field from both serialize and
    /// deserialize. The transient design makes the attribute behave
    /// as a one-time directive: an adopter who leaves
    /// `#[field(type_change_using = "...")]` on a field after the
    /// migration applies produces no phantom diff, because the loaded
    /// snapshot always carries `None` here while the freshly-projected
    /// schema carries the live attribute value.
    ///
    /// **Excluded from `PartialEq`.** See the manual
    /// `impl PartialEq for ColumnSchema` below for the rationale and
    /// the maintenance contract. The slot is exempt from structural
    /// equality so a load-vs-projection mismatch on
    /// `type_change_using` does not synthesise a phantom
    /// `AlterColumn`; only a real `sql_type` change carries the
    /// expression into a `ChangeType` operation.
    #[serde(default, skip)]
    pub type_change_using: Option<String>,
}

/// Manual equality for [`ColumnSchema`] that excludes the transient
/// [`ColumnSchema::type_change_using`] slot.
///
/// **Why manual.** Adopters declare `#[field(type_change_using = "...")]`
/// as a one-time migration directive — it tells the SQL emitter which
/// `USING (<expr>)` clause to append to an `ALTER COLUMN … TYPE`
/// statement and is never persisted to the snapshot. A derived
/// `PartialEq` would compare it against the loaded snapshot's `None`
/// and report inequality whenever the descriptor carries the
/// attribute. That false-positive ripples into every comparison
/// pathway that reaches `ColumnSchema::eq`:
///
/// 1. `diff_schemas`'s top-level `if before == after` short-circuit
///    would skip the NoOp return; the differ would walk every column
///    even when nothing structural changed.
/// 2. `diff_columns_in_table`'s `if bc == ac { continue; }` per-column
///    skip would not fire, dragging every column through
///    `emit_alter_column` for no structural reason.
/// 3. `build_match::schema_equiv`'s `a.models == b.models` recursion
///    compares `ColumnSchema` directly; a `models == pending` mismatch
///    here would route the three-way drift classifier to
///    `Outcome4PendingInvalid` (a spurious "pending JSON is stale"
///    warning) whenever an adopter had a `type_change_using`
///    attribute live in source after composing.
///
/// All three pathways are user-visible. Excluding the field from
/// equality at the type level keeps every downstream consumer correct
/// without each one having to remember to mask the slot.
///
/// **Maintenance.** Every other field must remain in the impl. Adding
/// a new persistent field to `ColumnSchema` requires extending this
/// impl in the same change so equality stays in sync with the struct.
/// The impl destructures both sides exhaustively so adding a field to
/// `ColumnSchema` without threading it here is a compile error rather
/// than a silent regression in differ behaviour.
impl PartialEq for ColumnSchema {
    fn eq(&self, other: &Self) -> bool {
        // Exhaustive destructure forces a compile error whenever a new
        // persistent field lands on `ColumnSchema` without being added
        // to the equality comparison below. The `..` rest pattern is
        // deliberately NOT used — the whole point of this impl is to
        // catch future maintainers who forget to thread a new field
        // through. Field order mirrors the struct definition so the
        // diff between this impl and the struct definition stays
        // legible.
        //
        // `type_change_using` is bound on both sides with the `_`
        // pattern to acknowledge the field exists while documenting
        // that it is deliberately excluded from equality (see this
        // impl's doc above for the rationale, and the field's own
        // doc on `ColumnSchema::type_change_using` for the transient-
        // slot design).
        let ColumnSchema {
            check: self_check,
            comment: self_comment,
            default_sql: self_default_sql,
            foreign_key: self_foreign_key,
            generated: self_generated,
            identity: self_identity,
            index_type: self_index_type,
            indexed: self_indexed,
            max_length: self_max_length,
            name: self_name,
            nullable: self_nullable,
            on_delete: self_on_delete,
            outbox_exclude: self_outbox_exclude,
            rationale: self_rationale,
            relation_kind: self_relation_kind,
            renamed_from: self_renamed_from,
            sequence_within: self_sequence_within,
            sql_type: self_sql_type,
            unique: self_unique,
            // Deliberately excluded from PartialEq — see impl doc.
            type_change_using: _,
        } = self;
        let ColumnSchema {
            check: other_check,
            comment: other_comment,
            default_sql: other_default_sql,
            foreign_key: other_foreign_key,
            generated: other_generated,
            identity: other_identity,
            index_type: other_index_type,
            indexed: other_indexed,
            max_length: other_max_length,
            name: other_name,
            nullable: other_nullable,
            on_delete: other_on_delete,
            outbox_exclude: other_outbox_exclude,
            rationale: other_rationale,
            relation_kind: other_relation_kind,
            renamed_from: other_renamed_from,
            sequence_within: other_sequence_within,
            sql_type: other_sql_type,
            unique: other_unique,
            // Deliberately excluded from PartialEq — see impl doc.
            type_change_using: _,
        } = other;
        self_check == other_check
            && self_comment == other_comment
            && self_default_sql == other_default_sql
            && self_foreign_key == other_foreign_key
            && self_generated == other_generated
            && self_identity == other_identity
            && self_index_type == other_index_type
            && self_indexed == other_indexed
            && self_max_length == other_max_length
            && self_name == other_name
            && self_nullable == other_nullable
            && self_on_delete == other_on_delete
            && self_outbox_exclude == other_outbox_exclude
            && self_rationale == other_rationale
            && self_relation_kind == other_relation_kind
            && self_renamed_from == other_renamed_from
            && self_sequence_within == other_sequence_within
            && self_sql_type == other_sql_type
            && self_unique == other_unique
    }
}

impl Eq for ColumnSchema {}

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
    /// Postgres extension name (e.g. `"btree_gist"`) that must be
    /// installed before the migration runs. `None` for stock GiST
    /// exclusions that only use range / geometric operators.
    ///
    /// The macro auto-derives `Some("btree_gist")` for `using = "gist"`
    /// exclusions whose element list contains at least one btree
    /// comparison operator (`=`, `<>`, `<`, `<=`, `>`, `>=`) — see
    /// [`crate::descriptor::ExclusionConstraintSpec::extension_dependency`].
    /// The bootstrap composer reads this slot to aggregate the per-
    /// database extension install list (djogi#148).
    ///
    /// `#[serde(default)]` so snapshots predating this field round-
    /// trip cleanly — older snapshots load as `None`; the differ then
    /// sees the projected `Some("btree_gist")` as a drop+add of the
    /// constraint, which is the deliberate "extension dependency
    /// changed" signal.
    #[serde(default)]
    pub extension_dependency: Option<String>,
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
    /// 64-bit time-ordered, ascending — `BIGINT DEFAULT heerid_next()`.
    HeerId,
    /// 64-bit recency-biased — most-recent-first BTree scans without
    /// a secondary descending index. `BIGINT DEFAULT
    /// heerid_next_desc()`.
    HeerIdRecencyBiased,
    /// 128-bit UUIDv8 ascending — `UUID DEFAULT ranjid_next()`.
    RanjId,
    /// 128-bit UUIDv8 recency-biased — `UUID DEFAULT
    /// ranjid_next_desc()`.
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
mod column_schema_type_change_using_tests {
    //! djogi#220 — `ColumnSchema::type_change_using` is a transient
    //! projection-only slot. These tests pin the three properties that
    //! make the design correct end-to-end:
    //!
    //! 1. Serde drops the slot on serialize (`#[serde(skip)]`).
    //! 2. Serde supplies the default `None` on deserialize.
    //! 3. The manual `PartialEq` impl excludes the slot from equality
    //!    so a freshly-projected `Some(...)` compares equal to a
    //!    loaded snapshot's `None` (zero phantom-diff cost when an
    //!    adopter leaves the attribute on the field after applying).

    use super::ColumnSchema;

    fn base_column(name: &str) -> ColumnSchema {
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
            nullable: false,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "TEXT".to_string(),
            unique: false,
            type_change_using: None,
        }
    }

    #[test]
    fn type_change_using_is_excluded_from_partial_eq() {
        // Two columns identical except for `type_change_using` must
        // compare equal. This is the load-vs-projection scenario the
        // differ relies on: the snapshot loaded from disk always
        // has `None` here (due to serde-skip), while the live
        // descriptor projection carries the adopter's `Some(...)`.
        // The manual `PartialEq` impl on `ColumnSchema` must treat
        // them as equal so the per-column diff short-circuit
        // (`if bc == ac { continue; }`) fires correctly.
        let snapshot_loaded = base_column("kind");
        let projected_from_descriptor = ColumnSchema {
            type_change_using: Some("kind::uuid".to_string()),
            ..base_column("kind")
        };
        assert_eq!(snapshot_loaded, projected_from_descriptor);
        // Sanity: the slot itself differs.
        assert_ne!(
            snapshot_loaded.type_change_using,
            projected_from_descriptor.type_change_using
        );
    }

    #[test]
    fn structural_difference_still_triggers_partial_eq_inequality() {
        // The manual impl is precise — it only masks
        // `type_change_using`. A real structural change (sql_type
        // here) must still surface inequality.
        let before = base_column("kind");
        let after = ColumnSchema {
            sql_type: "UUID".to_string(),
            type_change_using: Some("kind::uuid".to_string()),
            ..base_column("kind")
        };
        assert_ne!(
            before, after,
            "sql_type difference must trip PartialEq even when type_change_using is set"
        );
    }

    #[test]
    fn type_change_using_is_dropped_on_serialize() {
        // The persisted snapshot must never carry `type_change_using`.
        // Round-tripping through serde_json:
        //   1. serialize a column with `Some(expr)` → JSON
        //   2. inspect the JSON — must NOT contain the key
        //   3. deserialize the JSON back → `None`
        let with_using = ColumnSchema {
            type_change_using: Some("kind::uuid".to_string()),
            ..base_column("kind")
        };
        let json = serde_json::to_string(&with_using).expect("serialize");
        assert!(
            !json.contains("type_change_using"),
            "serialized JSON must not contain the transient slot: {json}"
        );
        let round_tripped: ColumnSchema = serde_json::from_str(&json).expect("deserialize");
        assert!(
            round_tripped.type_change_using.is_none(),
            "deserialize must yield None for the skipped slot: {round_tripped:?}"
        );
    }

    #[test]
    fn type_change_using_deserialize_defaults_when_absent() {
        // Snapshots predating this field (or written after it landed)
        // never carry the key. Loading them must yield `None`
        // structurally — confirms the `#[serde(default)]` slot has a
        // working Default::default() and the loader does not reject
        // the absence under `deny_unknown_fields`.
        let json = serde_json::to_string(&base_column("kind")).expect("serialize");
        let loaded: ColumnSchema = serde_json::from_str(&json).expect("deserialize");
        assert!(loaded.type_change_using.is_none());
    }
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
