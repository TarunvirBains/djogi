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
#[derive(Debug, Clone)]
pub struct IndexSpec {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub unique: bool,
    pub index_type: IndexType,
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
    /// Override the index method for this field's implicit index.
    /// `None` falls back to `IndexType::BTree`. Declared in Phase 1;
    /// migration generation for non-BTree methods lands in Phase 6.
    pub index_type: Option<IndexType>,
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
/// (Phase 5), projections (Phase 4.5 — `expose(...)` scope membership),
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
}

inventory::collect!(ModelDescriptor);
