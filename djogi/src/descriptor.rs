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

/// Subtype discriminator for `FieldSqlType::Geography`.
///
/// Phase 7's migration differ compares subtypes by discriminant, not by
/// `Display` text, so subtype renames or new variants do not surface as
/// spurious migration diffs.
///
/// Sealed via `#[non_exhaustive]` — adding a variant in a future phase is
/// not a breaking change for downstream consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeographySubtype {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiPolygon,
}

impl std::fmt::Display for GeographySubtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Point => "Point",
            Self::LineString => "LineString",
            Self::Polygon => "Polygon",
            Self::MultiPoint => "MultiPoint",
            Self::MultiPolygon => "MultiPolygon",
        })
    }
}

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
    /// PostGIS `geography(<subtype>, SRID)`. The `subtype` discriminant
    /// is a typed `GeographySubtype` so Phase 7's migration differ can
    /// compare subtypes by discriminant — subtype renames or new variants
    /// do not surface as spurious migration diffs.
    ///
    /// Phase 6 shipped with `Point` hardcoded in `Display`; T6 freezes the
    /// final descriptor shape that Phase 7 will consume.
    Geography {
        subtype: GeographySubtype,
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
            FieldSqlType::Geography { subtype, srid } => {
                write!(f, "geography({subtype}, {srid})")
            }
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
    use super::{
        FieldDescriptor, FieldSqlType, GeographySubtype, IndexSpec, IndexType, ModelDescriptor,
        PkType, migration_shape::MigrationShape,
    };

    // ── T6: GeographySubtype Display ─────────────────────────────────────────

    /// Phase 6 regression guard: `Geography { subtype: Point, srid: 4326 }`
    /// must emit exactly `"geography(Point, 4326)"` — unchanged from Phase 6
    /// where `"Point"` was hardcoded.
    #[test]
    fn geography_point_subtype_displays_unchanged_from_phase_6() {
        let ft = FieldSqlType::Geography {
            subtype: GeographySubtype::Point,
            srid: 4326,
        };
        assert_eq!(format!("{ft}"), "geography(Point, 4326)");
    }

    #[test]
    fn geography_linestring_subtype_displays_correctly() {
        let ft = FieldSqlType::Geography {
            subtype: GeographySubtype::LineString,
            srid: 4326,
        };
        assert_eq!(format!("{ft}"), "geography(LineString, 4326)");
    }

    #[test]
    fn geography_polygon_subtype_displays_correctly() {
        let ft = FieldSqlType::Geography {
            subtype: GeographySubtype::Polygon,
            srid: 4326,
        };
        assert_eq!(format!("{ft}"), "geography(Polygon, 4326)");
    }

    #[test]
    fn geography_multipoint_subtype_displays_correctly() {
        let ft = FieldSqlType::Geography {
            subtype: GeographySubtype::MultiPoint,
            srid: 4326,
        };
        assert_eq!(format!("{ft}"), "geography(MultiPoint, 4326)");
    }

    #[test]
    fn geography_multipolygon_subtype_displays_correctly() {
        let ft = FieldSqlType::Geography {
            subtype: GeographySubtype::MultiPolygon,
            srid: 4326,
        };
        assert_eq!(format!("{ft}"), "geography(MultiPolygon, 4326)");
    }

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

    /// Construct a minimal `ModelDescriptor` by hand and verify that
    /// `MigrationShape::from_descriptor` produces sensible column shapes for
    /// the framework-injected fields plus one user field.
    ///
    /// Hand-constructing a `ModelDescriptor` is feasible here because all
    /// fields are `&'static …` slices or options that can be satisfied with
    /// `'static` literals.  This unit test keeps the helper covered even
    /// without relying on `#[model]`-emitted data.
    #[test]
    fn migration_shape_from_minimal_descriptor() {
        use super::super::relation::{OnDelete, RelationKind};

        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                name: "id",
                sql_type: FieldSqlType::BigInt,
                nullable: false,
                unique: true,
                indexed: true,
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
            },
            FieldDescriptor {
                name: "label",
                sql_type: FieldSqlType::Text,
                nullable: false,
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
            },
        ];

        // Suppress unused-import warnings from the wildcard bring-in above —
        // `OnDelete` and `RelationKind` are imported because the `use` block
        // is required to satisfy the FieldDescriptor struct literal, even
        // though neither variant is used in the literal values.
        let _ = (OnDelete::Restrict, RelationKind::ForeignKey);

        let desc = ModelDescriptor {
            type_name: "Minimal",
            table_name: "minimals",
            pk_type: PkType::HeerId,
            fields: FIELDS,
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: &[],
            is_through: false,
            fts: None,
        };

        let shape = MigrationShape::from_descriptor(&desc);

        assert_eq!(shape.table_name, "minimals");
        assert!(
            shape.required_extensions.is_empty(),
            "no Geography fields → no required extensions"
        );
        assert!(
            shape.indexes.is_empty(),
            "empty IndexSpec slice → no IndexShape entries"
        );

        // Two columns in descriptor order.
        assert_eq!(shape.columns.len(), 2);
        assert_eq!(shape.columns[0].name, "id");
        assert_eq!(shape.columns[0].sql_type_text, "BIGINT");
        assert!(shape.columns[0].not_null);
        assert_eq!(shape.columns[1].name, "label");
        assert_eq!(shape.columns[1].sql_type_text, "TEXT");
        assert!(shape.columns[1].not_null);
    }

    // ── T11: has_gist_on_geography helper ────────────────────────────────────

    // Static descriptors used by has_gist_on_geography tests.
    // `ModelDescriptor` requires `'static` slices for `fields` and `indexes`,
    // so the entire data set must be declared as module-level statics rather
    // than local temporaries.

    static T11_GEO_FIELD: FieldDescriptor = FieldDescriptor {
        name: "boundary",
        sql_type: FieldSqlType::Geography {
            subtype: GeographySubtype::Polygon,
            srid: 4326,
        },
        nullable: false,
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
    };

    static T11_TEXT_FIELD: FieldDescriptor = FieldDescriptor {
        name: "label",
        sql_type: FieldSqlType::Text,
        nullable: false,
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
    };

    static T11_GIST_INDEX: IndexSpec = IndexSpec {
        name: "idx_boundary_gist",
        columns: &["boundary"],
        unique: false,
        index_type: IndexType::Gist,
        requires_out_of_transaction: true,
        extension_dependency: Some("postgis"),
    };

    static T11_BTREE_INDEX: IndexSpec = IndexSpec {
        name: "idx_boundary_btree",
        columns: &["boundary"],
        unique: false,
        index_type: IndexType::BTree,
        requires_out_of_transaction: false,
        extension_dependency: None,
    };

    static T11_GIST_ON_TEXT: IndexSpec = IndexSpec {
        name: "idx_label_gist",
        columns: &["label"],
        unique: false,
        index_type: IndexType::Gist,
        requires_out_of_transaction: false,
        extension_dependency: None,
    };

    /// A descriptor with a GiST index on a Geography field returns `true`.
    #[test]
    fn has_gist_on_geography_returns_true_when_indexed() {
        let desc = ModelDescriptor {
            type_name: "Region",
            table_name: "regions",
            pk_type: PkType::HeerId,
            fields: std::slice::from_ref(&T11_GEO_FIELD),
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: std::slice::from_ref(&T11_GIST_INDEX),
            is_through: false,
            fts: None,
        };
        assert!(
            desc.has_gist_on_geography(),
            "expected true: GiST index on a Geography field must be detected"
        );
    }

    /// A descriptor with no indexes at all returns `false`.
    #[test]
    fn has_gist_on_geography_returns_false_when_no_indexes() {
        let desc = ModelDescriptor {
            type_name: "Region",
            table_name: "regions",
            pk_type: PkType::HeerId,
            fields: std::slice::from_ref(&T11_GEO_FIELD),
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: &[],
            is_through: false,
            fts: None,
        };
        assert!(
            !desc.has_gist_on_geography(),
            "expected false: no indexes means no GiST-on-Geography"
        );
    }

    /// A descriptor with a BTree (not GiST) index on a Geography field
    /// returns `false` — only GiST is relevant for spatial acceleration.
    #[test]
    fn has_gist_on_geography_returns_false_for_btree_on_geography() {
        let desc = ModelDescriptor {
            type_name: "Region",
            table_name: "regions",
            pk_type: PkType::HeerId,
            fields: std::slice::from_ref(&T11_GEO_FIELD),
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: std::slice::from_ref(&T11_BTREE_INDEX),
            is_through: false,
            fts: None,
        };
        assert!(
            !desc.has_gist_on_geography(),
            "expected false: BTree index on Geography is not spatial acceleration"
        );
    }

    /// A GiST index on a non-Geography (text) column returns `false`.
    #[test]
    fn has_gist_on_geography_returns_false_for_gist_on_non_geo() {
        let desc = ModelDescriptor {
            type_name: "Region",
            table_name: "regions",
            pk_type: PkType::HeerId,
            fields: std::slice::from_ref(&T11_TEXT_FIELD),
            partition_by: None,
            has_outbox: false,
            idempotency_key: None,
            tenant_key: None,
            cache_ttl: None,
            rationale: None,
            indexes: std::slice::from_ref(&T11_GIST_ON_TEXT),
            is_through: false,
            fts: None,
        };
        assert!(
            !desc.has_gist_on_geography(),
            "expected false: GiST on a text column is not spatial acceleration"
        );
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
    /// Returns `true` if this model has a GiST index on at least one
    /// `Geography`-typed field.
    ///
    /// Used by [`crate::query::queryset::QuerySet::group_by_region`] at
    /// call time to warn when the region model has no spatial index, which
    /// causes the spatial JOIN to scan the region table linearly for every
    /// row in the data table.
    ///
    /// # Detection algorithm
    ///
    /// For each [`IndexSpec`] in `self.indexes`:
    /// 1. Skip entries whose `index_type` is not [`IndexType::Gist`].
    /// 2. For each column name in the spec's `columns` slice, check whether
    ///    the corresponding [`FieldDescriptor`] has
    ///    `sql_type == FieldSqlType::Geography { .. }`.
    /// 3. Return `true` as soon as one such matching field is found.
    ///
    /// Composite indexes count if **any** column in the index is
    /// `Geography`-typed. This reflects Postgres's GiST-prefix behaviour:
    /// a GiST index on `(boundary, other_col)` is still a valid spatial
    /// index that accelerates `ST_Contains` / `ST_DWithin` lookups on
    /// `boundary`, so we treat such composite indexes as satisfying the
    /// "has GiST on geography" check.
    ///
    /// Returns `false` if no GiST + Geography combination is found.
    pub fn has_gist_on_geography(&self) -> bool {
        for idx in self.indexes {
            if !matches!(idx.index_type, IndexType::Gist) {
                continue;
            }
            for col in idx.columns {
                let is_geo = self
                    .fields
                    .iter()
                    .find(|f| f.name == *col)
                    .map(|f| matches!(f.sql_type, FieldSqlType::Geography { .. }))
                    .unwrap_or(false);
                if is_geo {
                    return true;
                }
            }
        }
        false
    }

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

    /// Derive the migration-SQL intent implied by this descriptor.
    ///
    /// Returns a [`migration_shape::MigrationShape`] that captures every
    /// DDL decision the descriptor encodes — column SQL types, index DDL
    /// (including `CONCURRENTLY` placement for out-of-transaction indexes),
    /// and the set of Postgres extensions that must be present.
    ///
    /// This is a **contract helper**, not a runtime path.  Phase 7 will
    /// subsume it by emitting the same shape's content as actual migration
    /// SQL files.  Until then, contract tests assert against this structure
    /// to prove the descriptor encodes enough information for a downstream
    /// emitter to produce correct DDL without type-name inference.
    ///
    /// Visibility is `pub` so feature-gated integration tests can call it;
    /// Phase 7 may narrow it back to `pub(crate)` once the emitter owns this
    /// responsibility.
    pub fn migration_shape(&self) -> migration_shape::MigrationShape {
        migration_shape::MigrationShape::from_descriptor(self)
    }
}

inventory::collect!(ModelDescriptor);

/// Contract-validation helper that maps a [`ModelDescriptor`] to the DDL
/// intent it implies.
///
/// # Why this module exists
///
/// Phase 7 will emit actual `.sql` migration files.  Before Phase 7 lands,
/// this module proves the descriptor already encodes *all* information the
/// emitter will need: column SQL types (including PostGIS `GEOGRAPHY`),
/// per-index CONCURRENTLY placement, and required Postgres extensions.
/// Contract tests assert against [`MigrationShape`] values constructed from
/// macro-emitted `ModelDescriptor`s.
///
/// # Placement decision
///
/// The content is under 150 lines so it lives as an in-file submodule of
/// `descriptor.rs` rather than a sibling `descriptor/migration_shape.rs`
/// file.  This avoids a directory-split refactor and keeps the contract
/// helper adjacent to the types it describes.
pub mod migration_shape {
    use std::collections::BTreeSet;

    use super::{FieldSqlType, IndexType, ModelDescriptor};

    // -----------------------------------------------------------------------
    // Public types
    // -----------------------------------------------------------------------

    /// The migration-SQL intent implied by a [`ModelDescriptor`].
    ///
    /// T6 contract helper.  Phase 7 will subsume this by emitting the same
    /// structure's contents as actual DDL files.  Until then,
    /// `MigrationShape` is the typed proof that the descriptor encodes
    /// enough information for a downstream emitter to produce correct
    /// migration SQL without type-name inference.
    #[derive(Debug, Clone)]
    pub struct MigrationShape {
        /// The Postgres table name (`ModelDescriptor::table_name`).
        pub table_name: &'static str,
        /// One entry per descriptor field, in descriptor order.
        pub columns: Vec<ColumnShape>,
        /// One entry per `IndexSpec` in `ModelDescriptor::indexes`.
        pub indexes: Vec<IndexShape>,
        /// Postgres extensions that must be installed before this table's
        /// DDL runs.  Collected from:
        /// - every `IndexSpec::extension_dependency` that is `Some`
        /// - every field whose `sql_type` is `FieldSqlType::Geography`
        ///   (even if no index exists — the column itself requires PostGIS)
        pub required_extensions: BTreeSet<&'static str>,
    }

    /// DDL-relevant metadata for a single column.
    #[derive(Debug, Clone)]
    pub struct ColumnShape {
        /// Column name from `FieldDescriptor::name`.
        pub name: &'static str,
        /// SQL type string produced by `FieldSqlType`'s `Display` impl.
        ///
        /// Case matches the `Display` impl exactly:
        /// - Standard types are uppercased (`"TEXT"`, `"BIGINT"`, `"TIMESTAMPTZ"`).
        /// - `Geography { srid }` is lowercase-prefixed:
        ///   `"geography(Point, 4326)"`.
        ///
        /// The plan's prose example used `"GEOGRAPHY(Point, 4326)"` (uppercase
        /// prefix) as an illustration.  The actual `Display` impl uses lowercase
        /// `"geography(Point, 4326)"`.  Contract tests follow the Display impl —
        /// keeping one canonical text path is more important than matching the
        /// prose example's capitalisation.
        pub sql_type_text: String,
        /// `true` when `FieldDescriptor::nullable` is `false` (the column is
        /// `NOT NULL` in SQL).
        pub not_null: bool,
    }

    /// DDL-relevant metadata for a single index, plus the SQL the emitter
    /// would produce.
    #[derive(Debug, Clone)]
    pub struct IndexShape {
        /// Index name from `IndexSpec::name`.
        pub name: &'static str,
        /// Column list from `IndexSpec::columns`, converted to an owned `Vec`.
        pub columns: Vec<&'static str>,
        /// Whether the index is a `UNIQUE` constraint.
        pub unique: bool,
        /// Mirrors `IndexSpec::requires_out_of_transaction`.
        /// When `true`, `sql_text` will contain `CONCURRENTLY`.
        pub requires_out_of_transaction: bool,
        /// Mirrors `IndexSpec::extension_dependency`.
        pub extension_dependency: Option<&'static str>,
        /// The `CREATE INDEX` statement the Phase 7 emitter would produce.
        ///
        /// Not executed in Phase 6 — this is the contract proof.
        /// Index-type keyword is lowercase (`gist`, `gin`, `btree`, …).
        pub sql_text: String,
    }

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    impl MigrationShape {
        /// Walk a `ModelDescriptor` and produce the DDL intent it implies.
        pub fn from_descriptor(desc: &ModelDescriptor) -> Self {
            let table = desc.table_name;

            // ── Columns ────────────────────────────────────────────────────
            let columns: Vec<ColumnShape> = desc
                .fields
                .iter()
                .map(|f| ColumnShape {
                    name: f.name,
                    sql_type_text: f.sql_type.to_string(),
                    not_null: !f.nullable,
                })
                .collect();

            // ── Required extensions from fields ────────────────────────────
            let mut required_extensions: BTreeSet<&'static str> = BTreeSet::new();
            for f in desc.fields {
                if matches!(f.sql_type, FieldSqlType::Geography { .. }) {
                    required_extensions.insert("postgis");
                }
            }

            // ── Indexes ────────────────────────────────────────────────────
            let indexes: Vec<IndexShape> = desc
                .indexes
                .iter()
                .map(|spec| {
                    // Collect extension dependencies from indexes too.
                    if let Some(ext) = spec.extension_dependency {
                        required_extensions.insert(ext);
                    }

                    let type_kw = index_type_keyword(spec.index_type);
                    let cols_joined = spec.columns.join(",");

                    let create_kw = if spec.unique {
                        "CREATE UNIQUE INDEX"
                    } else {
                        "CREATE INDEX"
                    };
                    let concurrently = if spec.requires_out_of_transaction {
                        " CONCURRENTLY"
                    } else {
                        ""
                    };

                    let sql_text = format!(
                        "{create_kw}{concurrently} IF NOT EXISTS {name} ON {table} USING {type_kw}({cols_joined})",
                        name = spec.name,
                    );

                    IndexShape {
                        name: spec.name,
                        columns: spec.columns.to_vec(),
                        unique: spec.unique,
                        requires_out_of_transaction: spec.requires_out_of_transaction,
                        extension_dependency: spec.extension_dependency,
                        sql_text,
                    }
                })
                .collect();

            MigrationShape {
                table_name: table,
                columns,
                indexes,
                required_extensions,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Map an `IndexType` to its lowercase Postgres keyword.
    ///
    /// No `Display` impl is added to `IndexType` itself because the index
    /// keyword (`"gist"`) is a DDL-emission concern, not a general display
    /// concern.  This helper is local to the migration-shape module.
    fn index_type_keyword(t: IndexType) -> &'static str {
        match t {
            IndexType::BTree => "btree",
            IndexType::Gist => "gist",
            IndexType::Gin => "gin",
            IndexType::Hash => "hash",
            IndexType::Spgist => "spgist",
            IndexType::Brin => "brin",
        }
    }
}

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
