//! Runtime model descriptors — emitted by `#[model]` via `inventory`,
//! consumed by the migration system (Phase 6), `djogi docs` (Phase 6),
//! RLS generation (Phase 5), and the partitioning analyzer (Phase 7).
//!
//! # Why `ModelDescriptor` is the single source of truth
//!
//! The framework has several consumers that need to reason about a model's
//! schema at runtime or build time: the migration differ compares the live
//! set of `ModelDescriptor`s against `schema_snapshot.json`; the admin UI
//! renders columns from them; `djogi docs` turns them into markdown; the
//! RLS generator produces `CREATE POLICY` statements from `tenant_key`.
//! Rather than each subsystem re-parsing `#[model]` attributes, the proc
//! macro emits one `inventory::submit!(ModelDescriptor { ... })` per model
//! and every consumer iterates `inventory::iter::<ModelDescriptor>`.
//!
//! # Why the descriptor has forward-declared fields
//!
//! Some fields here (`partition_by`, `has_outbox`, `tenant_key`, `cache_ttl`,
//! `rationale`, `indexes`, plus per-field `outbox_exclude`, `index_type`,
//! `rationale`) are declared in Phase 1 but *populated* by later phases. They
//! default to `None` / `false` / `&[]` in Phase 1 so the struct layout is
//! stable across phases: adding a field later is a breaking change across
//! every `inventory::submit!` call site, which is exactly what the amendment
//! is designed to avoid.

// Re-exports of relation enums used in `FieldDescriptor`. Downstream consumers
// (migration differ, docs generator, admin UI) read `FieldDescriptor` and
// expect the referenced enums in scope via the descriptor module; exporting
// them here keeps the single-source-of-truth story consistent and avoids
// forcing every consumer to import from two paths.
pub use crate::relation::{OnDelete, RelationKind};
// Re-export of FTS descriptor — keeps the single-import story consistent for
// Phase 6's migration differ, which reads both relation and FTS metadata from
// the descriptor module path.
pub use crate::fts::FtsDescriptor;

/// SQL type a model field maps to.
///
/// This enum is the bridge between Rust field types and the column types
/// the migration system generates. The proc macro maps `String -> Text`,
/// `i64 -> BigInt`, `bool -> Boolean`, etc. User code rarely constructs
/// these directly — they appear in emitted `FieldDescriptor` literals.
///
/// `Custom` exists for types the framework doesn't know about (e.g.
/// `BYTEA`, `CITEXT`-derived domains, `geography(Polygon, 4326)`). The
/// migration differ treats `Custom("FOO")` and `Custom("FOO")` as equal
/// (string compare), so adding support for a new type is non-breaking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSqlType {
    Text,
    SmallInt,
    Integer,
    BigInt,
    Real,
    DoublePrecision,
    Boolean,
    Timestamptz,
    Date,
    Numeric,
    Uuid,
    Jsonb,
    TextArray,
    IntegerArray,
    BigIntArray,
    BoolArray,
    /// Case-insensitive text (Postgres `CITEXT`). Declared in Phase 1;
    /// used by the SQL linting plan in later phases.
    Citext,
    /// PostGIS `geography(Point, SRID)`. Declared in Phase 1;
    /// full wiring (codec, migration support) lands in later phases.
    Geography {
        srid: u32,
    },
    /// Fallback for SQL types the framework doesn't model explicitly.
    /// Stored verbatim and compared by string equality in the migration differ.
    Custom(&'static str),
}

impl std::fmt::Display for FieldSqlType {
    /// Used by `djogi docs` — produces clean SQL type names (`TEXT`, not
    /// `Text` or `Custom("BYTEA")`). Never change this to forward to
    /// `Debug`: the generated docs would become unreadable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldSqlType::Text => write!(f, "TEXT"),
            FieldSqlType::SmallInt => write!(f, "SMALLINT"),
            FieldSqlType::Integer => write!(f, "INTEGER"),
            FieldSqlType::BigInt => write!(f, "BIGINT"),
            FieldSqlType::Real => write!(f, "REAL"),
            FieldSqlType::DoublePrecision => write!(f, "DOUBLE PRECISION"),
            FieldSqlType::Boolean => write!(f, "BOOLEAN"),
            FieldSqlType::Timestamptz => write!(f, "TIMESTAMPTZ"),
            FieldSqlType::Date => write!(f, "DATE"),
            FieldSqlType::Numeric => write!(f, "NUMERIC"),
            FieldSqlType::Uuid => write!(f, "UUID"),
            FieldSqlType::Jsonb => write!(f, "JSONB"),
            FieldSqlType::TextArray => write!(f, "TEXT[]"),
            FieldSqlType::IntegerArray => write!(f, "INTEGER[]"),
            FieldSqlType::BigIntArray => write!(f, "BIGINT[]"),
            FieldSqlType::BoolArray => write!(f, "BOOLEAN[]"),
            FieldSqlType::Citext => write!(f, "CITEXT"),
            FieldSqlType::Geography { srid } => write!(f, "geography(Point, {srid})"),
            FieldSqlType::Custom(s) => write!(f, "{s}"),
        }
    }
}

/// Partition strategy for a model table.
///
/// Set via `#[model(partition_by = "range:created_at")]` or
/// `#[model(partition_by = "hash:id:8")]`. Phase 1 declares the enum so
/// `ModelDescriptor::partition_by` has a stable type; attribute parsing and
/// partition-aware `QuerySet` land in Phase 7 (partitioning plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionSpec {
    /// `PARTITION BY RANGE (column)` — typically used for time-series data.
    /// Requires pg_partman for automatic INTERVAL-based child table creation.
    Range { column: &'static str },
    /// `PARTITION BY HASH (column) PARTITIONS n` — distributes rows evenly.
    Hash {
        column: &'static str,
        partitions: u16,
    },
}

/// Index method Postgres uses for a column or composite index.
///
/// Phase 1 declares; Phase 6's migration differ emits `USING btree|gin|gist|...`
/// based on this field. The enum covers every method Postgres ships — `BRIN`
/// is included for the partitioning plan's time-series optimization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    BTree,
    Gist,
    Gin,
    Hash,
    Spgist,
    Brin,
}

/// Named index declaration.
///
/// `ModelDescriptor::indexes` is `&[IndexSpec]` so a model can declare
/// composite / non-BTree indexes that the migration differ turns into
/// `CREATE INDEX` statements. An empty slice — the Phase 1 default — means
/// "only the implicit PK and per-field `#[field(index)]` indexes".
///
/// # Phase 6 migration-policy fields
///
/// Two fields were added in Phase 6 to carry DDL-emission intent directly in
/// the descriptor rather than having Phase 7 reverse-engineer intent from
/// type names:
///
/// - `requires_out_of_transaction` — when `true`, the migration emitter must
///   run the index DDL outside any implicit transaction wrapper (i.e.
///   `CREATE INDEX CONCURRENTLY`). GiST indexes on large tables typically
///   need this. Non-spatial indexes default to `false`.
///
/// - `extension_dependency` — names a required Postgres extension (e.g.
///   `"postgis"`) that must be present before the index DDL runs. The
///   migration emitter inserts a `CREATE EXTENSION IF NOT EXISTS <ext>`
///   guard before the index statement when this is `Some`. Non-spatial
///   indexes default to `None`.
///
/// Use [`IndexSpec::simple`] to construct non-spatial indexes without listing
/// these two fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub unique: bool,
    pub index_type: IndexType,
    /// When `true`, the migration emitter must place this index DDL outside
    /// any implicit transaction (e.g. `CREATE INDEX CONCURRENTLY`). Set to
    /// `true` for GiST indexes on PostGIS `GEOGRAPHY` columns.
    pub requires_out_of_transaction: bool,
    /// Postgres extension name (e.g. `"postgis"`) that must be installed
    /// before this index can be created. `None` for standard BTree / GIN / …
    /// indexes that have no extension dependency.
    pub extension_dependency: Option<&'static str>,
}

impl IndexSpec {
    /// Backward-compatible constructor for non-spatial indexes.
    ///
    /// Defaults `requires_out_of_transaction = false` and
    /// `extension_dependency = None` so call sites that predated Phase 6 can
    /// remain on the 4-argument shape without listing the two new fields.
    ///
    /// This constructor is `const` so it can be used in `const` contexts and
    /// in `&[IndexSpec::simple(...)]` literal arrays.
    pub const fn simple(
        name: &'static str,
        columns: &'static [&'static str],
        unique: bool,
        index_type: IndexType,
    ) -> Self {
        Self {
            name,
            columns,
            unique,
            index_type,
            requires_out_of_transaction: false,
            extension_dependency: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{IndexSpec, IndexType};

    #[test]
    fn simple_constructor_defaults_policy_fields_to_benign() {
        let spec = IndexSpec::simple("idx", &["col"], false, IndexType::BTree);
        assert!(
            !spec.requires_out_of_transaction,
            "simple() must default requires_out_of_transaction to false"
        );
        assert_eq!(
            spec.extension_dependency, None,
            "simple() must default extension_dependency to None"
        );
        // Spot-check that the positional fields were forwarded correctly.
        assert_eq!(spec.name, "idx");
        assert_eq!(spec.columns, &["col"]);
        assert!(!spec.unique);
        assert_eq!(spec.index_type, IndexType::BTree);
    }
}

/// Metadata for a single model field.
///
/// `ModelDescriptor::fields` is the complete schema contract — it
/// INCLUDES the framework-injected columns (`id`, `created_at`,
/// `updated_at`) before any user-declared fields. Consumers
/// (migration differ, admin UI, `djogi docs`, RLS generator) iterate
/// `descriptor.fields` as the single schema source and never
/// synthesize framework columns out-of-band.
///
/// Field order: `id` (omitted for `pk = "none"`), then `created_at`,
/// then `updated_at`, then user fields in source order.
#[derive(Debug, Clone)]
pub struct FieldDescriptor {
    pub name: &'static str,
    pub sql_type: FieldSqlType,
    pub nullable: bool,
    pub unique: bool,
    pub indexed: bool,
    pub max_length: Option<u32>,
    pub renamed_from: Option<&'static str>,
    /// Human-readable rationale for non-obvious field design choices.
    /// Set via `#[field(rationale = "...")]`. The proc macro emits an
    /// advisory warning (Phase 5) when `lazy = true` or `outbox = "ignore"`
    /// is set without a rationale — surfaces forgotten context at review time.
    pub rationale: Option<&'static str>,
    /// `#[field(outbox = "ignore")]` — exclude the field from the outbox
    /// payload. Declared in Phase 1; outbox writes land in Phase 5.
    pub outbox_exclude: bool,
    /// `#[field(sequence_within = "parent_fk_column")]` — parent FK
    /// column that scopes this field's monotonic sequence. When
    /// `Some(col)`, the emitted `Model::create` runs a counter
    /// upsert against `<table>_seq_<col>` inside the caller's
    /// atomic scope before inserting the row, captures the returned
    /// `last_seq`, and writes it into this field. Phase 4 Task 7.6.
    ///
    /// `None` for every field that is not scope-sequenced. The
    /// macro enforces that at most one field per model carries this
    /// attribute (multi-scope sequencing is a future extension).
    pub sequence_within: Option<&'static str>,
    /// Override the index method for this field's implicit index.
    /// `None` falls back to `IndexType::BTree`. Declared in Phase 1;
    /// migration generation for non-BTree methods lands in Phase 6.
    pub index_type: Option<IndexType>,

    // ── Relation metadata (Phase 3 Task 2) ────────────────────────────────
    /// Relation cardinality, when this field stores the `Source`-side column
    /// of an FK / O2O relation. `None` for every scalar column — the macro
    /// recognises only `ForeignKey<T>`, `Option<ForeignKey<T>>`,
    /// `OneToOneField<T>`, and `Option<OneToOneField<T>>` as relation shapes;
    /// anything else keeps this at `None` and the downstream consumers
    /// (Phase 6 DDL, Phase 4 prefetch planning) treat the column as a
    /// scalar.
    pub relation_kind: Option<RelationKind>,

    /// `#[field(on_delete = "...")]` value, meaningful only when
    /// `relation_kind.is_some()`. A `None` here with `relation_kind = Some`
    /// falls back to `OnDelete::Restrict` at DDL-emission time (Phase 6)
    /// — matching the framework's cascade-off-by-default stance. The
    /// descriptor stores the parsed value (not the raw string) so every
    /// downstream consumer works from the same enum.
    pub on_delete: Option<OnDelete>,

    /// Fully-qualified target type name (e.g. `"Owner"` for
    /// `ForeignKey<Owner>`). `None` for scalar columns. Used by the Phase 6
    /// migration emitter to produce `REFERENCES {target_table}(id)` clauses,
    /// and by the Phase 4 prefetch planner when it needs to reflect on the
    /// target's `ModelDescriptor`. Stored as the Rust type name — not a
    /// table name — so it can be matched against the registered
    /// `ModelDescriptor::type_name` without re-deriving the identifier.
    pub target_type_name: Option<&'static str>,

    /// Forward-declared visage-per-scope mapping. Phase 3 emits an
    /// empty slice; Phase 4.5 extends `#[field(expose(scope = "column"))]`
    /// parsing to populate this without reshaping the descriptor. The
    /// slice shape is `&[(scope_name, emitted_column_alias)]` — the
    /// visage emitter projects the column under the aliased name
    /// when the given scope is active.
    pub visage_map: &'static [(&'static str, &'static str)],
}

/// Primary key strategy.
///
/// The four leaf variants (`HeerId`, `RanjId`, `Serial`, `None`) map 1:1 to
/// the `#[model(pk = "...")]` attribute values. `Composite` is emitted for
/// models that declare multiple PK columns — rare, mostly join tables — and
/// carries the ordered list of column names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkType {
    HeerId,
    RanjId,
    Serial,
    None,
    Composite(&'static [&'static str]),
}

/// Full descriptor for a registered model — collected via `inventory::submit!`.
///
/// This is the single source of truth for every framework subsystem that
/// reflects on schema: migrations (Phase 6), `djogi docs` (Phase 6), RLS
/// generation (Phase 5), partitioning (Phase 7), the outbox system
/// (Phase 5), visages (Phase 4.5 — `expose(...)` scope membership),
/// protected-data governance (Phase 6.5 — sensitivity, codecs, redaction),
/// data-lifecycle planning (Phase 8.5), and distributed topology
/// (Phase 10 — shard/residency/placement metadata). Extending this struct
/// is a coordinated action across all call sites — every
/// `inventory::submit!` generated by the macro must fill every field.
///
/// `fields` contains the **complete** column set: framework-injected columns
/// (`id`, `created_at`, `updated_at`) appear before user-declared fields in
/// injection order. See [`FieldDescriptor`] for the exact ordering contract.
#[derive(Debug)]
pub struct ModelDescriptor {
    pub type_name: &'static str,
    pub table_name: &'static str,
    pub pk_type: PkType,
    pub fields: &'static [FieldDescriptor],

    // ── Partitioning (items 18–23, partitioning-write-contention plan) ──────
    /// Partition strategy. `None` = no partitioning (most models).
    /// Set via `#[model(partition_by = "hash:id:8")]` or `"range:created_at"`.
    pub partition_by: Option<PartitionSpec>,

    // ── Outbox / Events (items 26–30, realtime-tenancy-outbox plan) ─────────
    /// True when `#[model(events)]` is set.
    /// Enables `_in_tx` CRUD variants that write to `{table}_outbox`.
    pub has_outbox: bool,

    // ── Idempotency (item 28, realtime-tenancy-outbox plan) ─────────────────
    /// Column name used as the idempotency key for `create_or_find()`.
    /// Set via `#[model(idempotency_key = "request_id")]`.
    pub idempotency_key: Option<&'static str>,

    // ── Multi-Tenancy / RLS (item 25, realtime-tenancy-outbox plan) ─────────
    /// Column name used as the Row Level Security tenant isolation key.
    /// Set via `#[model(tenant_key = "org_id")]`.
    /// When set, `query()` warns if `set_tenant()` has not been called.
    /// `query_insecurely()` bypasses the check (Phase 5).
    pub tenant_key: Option<&'static str>,

    // ── Redis Write-Through Cache (item 29, realtime-tenancy-outbox plan) ────
    /// Cache TTL in seconds for the Write-Through Redis cache.
    /// Set via `#[model(cache_ttl = 60)]`.
    /// `None` = no caching (default for all models).
    pub cache_ttl: Option<u32>,

    // ── Intent / Security (items 31–38, security-intent-docs plan) ──────────
    /// Human-readable rationale for why this model exists or non-obvious
    /// design choices. Set via `#[model(rationale = "...")]`. Included in
    /// `djogi docs` output. The proc macro emits an advisory warning when
    /// `partition_by` is set without a rationale (Phase 5).
    pub rationale: Option<&'static str>,

    // ── Indexes (items 43–46, graph-spatial-advanced-indexing plan) ─────────
    /// Named index declarations. Phase 1 emits an empty slice by default.
    /// Migration generation for these lands in Phase 6.
    pub indexes: &'static [IndexSpec],

    // ── Many-to-many (Task 6, phase3-relations plan) ────────────────────────
    /// `true` when the model is a `#[model(table = "...", through)]`
    /// junction table for a specific `impl ManyToMany<Target> for Source`.
    ///
    /// Through models remain ordinary queryable `Model`s — this flag is
    /// purely a marker carried in the descriptor for downstream consumers:
    ///
    /// - Phase 6's migration differ can suppress standalone admin /
    ///   routing affordances for through tables (deferred).
    /// - Human-facing tools (`djogi docs`, the shell's `.list_models`)
    ///   can hide through tables from the primary model list.
    ///
    /// `#[derive(Model)]` without `through` sets this to `false`.
    pub is_through: bool,

    // ── Full-Text Search (Phase 5 Task 14) ──────────────────────────────────
    /// Full-text search configuration when `#[model(fts = { source = "...",
    /// dictionary = "..." })]` is set.
    ///
    /// `None` for every model that does not declare an FTS column (the
    /// default). `Some(spec)` means a `GENERATED ALWAYS AS` tsvector column
    /// is expected in the schema and a GIN index should accompany it.
    ///
    /// # Phase 6 migration differ — important note
    ///
    /// **Changing `FtsDescriptor.dictionary` is a column-type alteration.**
    /// The generated column expression embeds the dictionary name literally:
    /// `to_tsvector('<dictionary>', <source>)`. Altering the dictionary
    /// requires dropping and re-creating the generated column — the migration
    /// differ must treat this field the same way it treats a `FieldSqlType`
    /// change (drop + add, not an in-place ALTER). Differ authors: compare
    /// `old_desc.fts` with `new_desc.fts` using `PartialEq` — any difference
    /// in `column`, `source`, or `dictionary` requires a column reconstruction.
    pub fts: Option<FtsDescriptor>,
}

impl ModelDescriptor {
    /// The primary key column name for this model.
    ///
    /// Returns `Some("id")` for the three standard PK types (`HeerId`,
    /// `RanjId`, `Serial`) and `None` for `pk = "none"` models. `Composite`
    /// PKs are uncommon; this method returns the first column in the composite
    /// list on the assumption that it is the most natural tiebreak candidate.
    ///
    /// Used by [`crate::query::order::OrderExpr::spatial_distance_with_pk_tiebreak`]
    /// to capture the PK column at `order_by_distance` construction time.
    pub fn pk_column(&self) -> Option<&'static str> {
        match &self.pk_type {
            PkType::HeerId | PkType::RanjId | PkType::Serial => Some("id"),
            PkType::None => None,
            PkType::Composite(cols) => cols.first().copied(),
        }
    }
}

inventory::collect!(ModelDescriptor);

/// Full descriptor for a registered Postgres enum type — collected via `inventory::submit!`.
///
/// `#[derive(DjogiEnum)]` emits one `inventory::submit!(EnumDescriptor { ... })` per enum.
/// The Phase 7 migration differ consumes these via `inventory::iter::<EnumDescriptor>()` to
/// emit `CREATE TYPE <postgres_type> AS ENUM (...)` DDL statements.
///
/// # Layout
///
/// - `type_name` — Rust type name as a string (`"VehicleStatus"`). Used by the migration
///   differ and `djogi docs` to identify the origin type.
/// - `postgres_type` — Postgres type name from `#[djogi_enum(name = "...")]`
///   (`"vehicle_status"`). This is the value passed to `CREATE TYPE ... AS ENUM`.
/// - `variants` — mapped string labels in declaration order. These are the wire values that
///   appear in the Postgres `ENUM` definition and in every serialized row.
///
/// Phase 7 owns DDL emission; Phase 5 only supplies the descriptor so the collector is
/// populated and ready for migration consumers.
#[derive(Debug)]
pub struct EnumDescriptor {
    /// Rust type name — e.g. `"VehicleStatus"`.
    pub type_name: &'static str,
    /// Postgres enum type name — e.g. `"vehicle_status"`.
    pub postgres_type: &'static str,
    /// Mapped variant strings in declaration order — e.g. `&["active", "in_maintenance", "decommissioned"]`.
    pub variants: &'static [&'static str],
}

inventory::collect!(EnumDescriptor);
