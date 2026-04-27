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

/// Uniqueness discipline for an [`IndexSpec`] — introduced in Phase 7-Zero
/// v3 to replace the binary `unique: bool` field with a three-valued enum.
///
/// Postgres distinguishes between a `UNIQUE` constraint (declared on the
/// table; created with a backing unique index; references the index by name)
/// and a `UNIQUE INDEX` (a plain index that happens to enforce uniqueness
/// without participating in the constraint catalogue). The former is what
/// most users mean when they say "unique"; the latter is what you reach for
/// when you need `WHERE ... IS NOT NULL` partial uniqueness or
/// `NULLS NOT DISTINCT` semantics that the constraint form does not expose.
///
/// Variant map:
/// - [`IndexKind::NonUnique`] — a plain index. The typical case.
/// - [`IndexKind::UniqueConstraint`] — `UNIQUE` constraint on the table.
///   `IndexSpec::simple(..., unique = true, ...)` maps to this variant.
/// - [`IndexKind::UniqueIndex`] — `CREATE UNIQUE INDEX` without a constraint
///   row. Required when [`IndexSpec::predicate`] is set or when
///   [`IndexSpec::nulls_not_distinct`] is `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    NonUnique,
    UniqueConstraint,
    UniqueIndex,
}

/// Sort direction for a single column inside an [`IndexColumnSpec`].
///
/// Column order is schema-significant: an index on `(a ASC, b DESC)` accelerates
/// a different set of `ORDER BY` queries than one on `(a DESC, b ASC)`. The
/// migration differ therefore treats per-column order as a meaningful field
/// rather than collapsing it away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOrder {
    Asc,
    Desc,
}

/// `NULLS FIRST` / `NULLS LAST` policy for a single column inside an
/// [`IndexColumnSpec`].
///
/// `IndexNullsOrder::Default` is the Postgres default (`NULLS LAST` for
/// `ASC`, `NULLS FIRST` for `DESC`) — use it when the user has not expressed
/// a preference so the emitter omits the `NULLS …` clause entirely and the
/// index remains a straight-forward structural match with the table DDL
/// Postgres itself would print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexNullsOrder {
    /// Postgres default — ASC implies NULLS LAST, DESC implies NULLS FIRST.
    /// The emitter does not print an explicit `NULLS …` clause for this
    /// variant.
    Default,
    First,
    Last,
}

/// Per-column knobs carried inside an [`IndexTarget::Columns`] entry.
///
/// Postgres indexes carry per-column sort direction, per-column nulls
/// ordering, and per-column opclass — flattening these onto the enclosing
/// [`IndexSpec`] (as an earlier v3 draft did) would make multi-column
/// indexes with mixed direction or mixed opclass impossible to express
/// without breaking the descriptor contract later. Keeping them on the
/// column spec leaves the contract additively-extensible for 0.1.0 and
/// beyond.
///
/// For the common "one simple column, no per-column knobs" case, use
/// [`IndexColumnSpec::simple`]; it fills in `opclass: None`, `order: Asc`,
/// `nulls: Default` so declarations stay one-liners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexColumnSpec {
    pub name: &'static str,
    /// Per-column Postgres opclass, e.g. `"text_pattern_ops"` on a `LIKE`
    /// acceleration index. `None` lets Postgres pick the default opclass
    /// for the column's data type.
    pub opclass: Option<&'static str>,
    pub order: IndexOrder,
    pub nulls: IndexNullsOrder,
}

impl IndexColumnSpec {
    /// Ergonomic constructor for the common case: name-only, `Asc`, default
    /// nulls, no opclass. Multi-column simple indexes stay one-liners:
    ///
    /// ```ignore
    /// IndexTarget::Columns(&[
    ///     IndexColumnSpec::simple("first"),
    ///     IndexColumnSpec::simple("last"),
    /// ])
    /// ```
    pub const fn simple(name: &'static str) -> Self {
        Self {
            name,
            opclass: None,
            order: IndexOrder::Asc,
            nulls: IndexNullsOrder::Default,
        }
    }
}

/// Target — column list or expression — that an [`IndexSpec`] covers.
///
/// The two forms are mutually exclusive by enum construction; an index
/// cannot simultaneously be a column-list index and an expression index.
/// Expression-target indexes do **not** support per-column opclass in
/// 0.1.0 — drop to raw SQL via `ctx.raw_execute(...)` if you need that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexTarget {
    Columns(&'static [IndexColumnSpec]),
    Expression(&'static str),
}

/// Named index declaration.
///
/// `ModelDescriptor::indexes` is `&[IndexSpec]` so a model can declare
/// composite / non-BTree indexes that the migration differ turns into
/// `CREATE INDEX` statements. An empty slice — the Phase 1 default — means
/// "only the implicit PK and per-field `#[field(index)]` indexes".
///
/// # Phase 7-Zero v3 shape
///
/// Phase 7-Zero widened the contract from a `(columns, unique)` pair into
/// a richer structure that can express the full Postgres index surface
/// without further breaking changes:
///
/// - `target` replaces `columns` and uses [`IndexTarget`] to pick either a
///   per-column list ([`IndexTarget::Columns`]) or an expression
///   ([`IndexTarget::Expression`]).
/// - `kind` replaces `unique: bool` with [`IndexKind`] so partial / nulls-
///   not-distinct unique indexes stop being forced through the constraint
///   form.
/// - `predicate`, `include`, and `nulls_not_distinct` are new optional
///   fields matching the Postgres DDL vocabulary.
/// - `requires_out_of_transaction` and `extension_dependency` from Phase 6
///   are preserved unchanged.
///
/// Use [`IndexSpec::simple`] to construct a plain column-list index without
/// listing every optional field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    pub name: &'static str,
    pub target: IndexTarget,
    pub kind: IndexKind,
    pub index_type: IndexType,
    /// Partial-index `WHERE` clause, e.g. `"deleted_at IS NULL"`. Raw SQL —
    /// emitted verbatim. `None` for a full-table index.
    pub predicate: Option<&'static str>,
    /// Columns to attach via `INCLUDE(...)` — non-key payload columns that
    /// let index-only scans answer more queries. Empty slice when unused.
    pub include: &'static [&'static str],
    /// When `true`, the emitted `CREATE UNIQUE INDEX` carries
    /// `NULLS NOT DISTINCT`. Forces `IndexKind::UniqueIndex` (constraint form
    /// does not expose this knob). `false` everywhere else.
    pub nulls_not_distinct: bool,
    /// When `true`, the migration emitter must place this index DDL outside
    /// any implicit transaction (e.g. `CREATE INDEX CONCURRENTLY`). Set to
    /// `true` for GiST indexes on PostGIS `GEOGRAPHY` columns and for any
    /// index declared with `concurrently = true` at the model level.
    pub requires_out_of_transaction: bool,
    /// Postgres extension name (e.g. `"postgis"`) that must be installed
    /// before this index can be created. `None` for standard BTree / GIN / …
    /// indexes that have no extension dependency.
    pub extension_dependency: Option<&'static str>,
}

/// Which flavour of index is being named — drives the stem selection in
/// [`index_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexNameKind {
    /// `<table>_<cols>_idx` — the default for `index(...)` declarations.
    NonUnique,
    /// `<table>_<cols>_key` — the default for `unique(...)` declarations
    /// that lower to `ADD CONSTRAINT ... UNIQUE`.
    UniqueConstraint,
    /// `<table>_<cols>_uidx` — used when uniqueness is enforced via
    /// `CREATE UNIQUE INDEX` rather than a constraint (partial,
    /// nulls-not-distinct, or other unique-index-only features).
    UniqueIndex,
}

/// Target shape for [`index_name`] — column-list or expression.
///
/// Expression-form indexes do not render the expression into the name
/// (expressions can be arbitrarily complex SQL and embedding them
/// defeats the 63-byte limit). Instead the stem becomes `_expr_idx` /
/// `_expr_uidx` and the hash suffix guarantees uniqueness across
/// multiple expression indexes on the same table.
#[derive(Debug, Clone, Copy)]
pub enum IndexNameTarget<'a> {
    /// Column-list form — each entry is one column name in declaration
    /// order. Order is semantic: `["last", "first"]` and `["first",
    /// "last"]` produce different names byte-for-byte.
    Columns(&'a [&'a str]),
    /// Expression form — the expression text is **not** included in the
    /// generated name; only the `expr` stem is used.
    Expression,
}

/// Compute the deterministic index name for a Phase 7 migration emitter.
///
/// Shape: `<table>_<stem-body>_<suffix>` where:
///
/// - `<stem-body>` is either the underscore-joined column names (for
///   [`IndexNameTarget::Columns`]) or the literal `expr` (for
///   [`IndexNameTarget::Expression`]).
/// - `<suffix>` is `idx` / `key` / `uidx` per [`IndexNameKind`].
///
/// Truncation rule (plan §D5): when the naïve name would exceed the
/// Postgres 63-byte identifier limit, the stem is truncated to 55 bytes
/// and an 8-character hex digest of the full pre-truncation name is
/// appended so near-duplicate inputs cannot collide.
///
/// The hash uses `std::hash::DefaultHasher` (SipHash-1-3) — determinism
/// within a single process is sufficient because the name is computed
/// once, emitted into a `static` literal, and never re-hashed at
/// runtime.
///
/// # Examples
///
/// ```ignore
/// use djogi::descriptor::{IndexNameKind, IndexNameTarget, index_name};
///
/// // Short, plain columns → verbatim `<table>_<cols>_idx`.
/// let name = index_name("users", IndexNameKind::NonUnique,
///     IndexNameTarget::Columns(&["email"]));
/// assert_eq!(name, "users_email_idx");
///
/// // Unique constraint → `_key` stem.
/// assert_eq!(
///     index_name("orgs", IndexNameKind::UniqueConstraint,
///         IndexNameTarget::Columns(&["org_id", "external_id"])),
///     "orgs_org_id_external_id_key"
/// );
///
/// // Expression index — table name + `expr` stem (hash suffix appears
/// // here only if the `<table>_expr_idx` string exceeds 63 bytes).
/// assert_eq!(
///     index_name("users", IndexNameKind::NonUnique,
///         IndexNameTarget::Expression),
///     "users_expr_idx"
/// );
/// ```
pub fn index_name(table: &str, kind: IndexNameKind, target: IndexNameTarget<'_>) -> String {
    let suffix = match kind {
        IndexNameKind::NonUnique => "idx",
        IndexNameKind::UniqueConstraint => "key",
        IndexNameKind::UniqueIndex => "uidx",
    };
    let body = match target {
        IndexNameTarget::Columns(cols) => cols.join("_"),
        IndexNameTarget::Expression => "expr".to_string(),
    };
    let full = format!("{table}_{body}_{suffix}");
    if full.len() <= 63 {
        return full;
    }
    // Truncate to 55 bytes and append an 8-char hex digest of the full
    // pre-truncation name. The byte-slice take is safe because `full` is
    // ASCII (table + body + suffix are all ASCII-ident-shape by Q5).
    let digest = {
        use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
        let mut h = BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
            .build_hasher();
        h.write(full.as_bytes());
        let raw = h.finish();
        format!("{:08x}", (raw as u32))
    };
    let stem: String = full.as_bytes()[..55].iter().map(|b| *b as char).collect();
    format!("{stem}_{digest}")
}

impl IndexSpec {
    /// Backward-compatible constructor for plain column-list indexes.
    ///
    /// Lifts each `&str` in `columns` into an [`IndexColumnSpec::simple`]
    /// entry, maps `unique = true` → [`IndexKind::UniqueConstraint`] and
    /// `unique = false` → [`IndexKind::NonUnique`], and defaults every other
    /// optional field (`predicate`, `include`, `nulls_not_distinct`,
    /// `requires_out_of_transaction`, `extension_dependency`) to benign values.
    ///
    /// Not `const`: the per-column spec slice is allocated on the heap via
    /// `Box::leak` so the lifted slice satisfies the `&'static` bound that
    /// `IndexTarget::Columns` requires. The leak is intentional — `IndexSpec`
    /// values are descriptor-lifetime data that lives for the entire process,
    /// so a once-per-index leak behaves like a `static` initialiser. For
    /// truly `static` contexts (macro-emitted descriptors), construct an
    /// `IndexSpec { ... }` literal directly and put the `IndexColumnSpec`
    /// slice behind a `static` binding.
    pub fn simple(
        name: &'static str,
        columns: &'static [&'static str],
        unique: bool,
        index_type: IndexType,
    ) -> Self {
        let lifted: Box<[IndexColumnSpec]> =
            columns.iter().map(|c| IndexColumnSpec::simple(c)).collect();
        let leaked: &'static [IndexColumnSpec] = Box::leak(lifted);
        let kind = if unique {
            IndexKind::UniqueConstraint
        } else {
            IndexKind::NonUnique
        };
        Self {
            name,
            target: IndexTarget::Columns(leaked),
            kind,
            index_type,
            predicate: None,
            include: &[],
            nulls_not_distinct: false,
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
        assert!(matches!(spec.kind, super::IndexKind::NonUnique));
        assert_eq!(spec.index_type, IndexType::BTree);
        match spec.target {
            super::IndexTarget::Columns(cols) => {
                assert_eq!(cols.len(), 1);
                assert_eq!(cols[0].name, "col");
            }
            super::IndexTarget::Expression(_) => panic!("expected Columns target"),
        }
        // New-in-7-Zero benign defaults round-trip.
        assert_eq!(spec.predicate, None);
        assert!(spec.include.is_empty());
        assert!(!spec.nulls_not_distinct);
    }

    // ── T1 (Phase 7-Zero v3) — new descriptor-shape assertions ───────────────

    #[test]
    fn index_kind_has_three_variants() {
        use super::IndexKind;
        // Compile-time existence + exhaustive-match coverage. If a variant
        // is renamed or removed, this match fails to compile.
        let variants = [
            IndexKind::NonUnique,
            IndexKind::UniqueConstraint,
            IndexKind::UniqueIndex,
        ];
        for v in &variants {
            match v {
                IndexKind::NonUnique | IndexKind::UniqueConstraint | IndexKind::UniqueIndex => {}
            }
        }
        assert_eq!(variants.len(), 3);
    }

    #[test]
    fn index_order_and_nulls_order_variants_exist() {
        use super::{IndexNullsOrder, IndexOrder};
        // Exhaustive coverage: if a variant is renamed or removed, these
        // matches stop compiling. The `let _ = ...` lines also pin the
        // variant construction surface.
        let orders = [IndexOrder::Asc, IndexOrder::Desc];
        for o in &orders {
            match o {
                IndexOrder::Asc | IndexOrder::Desc => {}
            }
        }
        assert_eq!(orders.len(), 2);

        let nulls = [
            IndexNullsOrder::Default,
            IndexNullsOrder::First,
            IndexNullsOrder::Last,
        ];
        for n in &nulls {
            match n {
                IndexNullsOrder::Default | IndexNullsOrder::First | IndexNullsOrder::Last => {}
            }
        }
        assert_eq!(nulls.len(), 3);
    }

    #[test]
    fn index_column_spec_simple_has_benign_defaults() {
        use super::{IndexColumnSpec, IndexNullsOrder, IndexOrder};
        let c = IndexColumnSpec::simple("last");
        assert_eq!(c.name, "last");
        assert_eq!(c.opclass, None);
        assert!(matches!(c.order, IndexOrder::Asc));
        assert!(matches!(c.nulls, IndexNullsOrder::Default));
    }

    #[test]
    fn index_target_is_mutually_exclusive_enum() {
        use super::{IndexColumnSpec, IndexTarget};
        // Mutual exclusion at the type level: both arms inhabit the same
        // enum so only one form can be stored per IndexSpec.
        static COLS: &[IndexColumnSpec] = &[IndexColumnSpec::simple("a")];
        let columns = IndexTarget::Columns(COLS);
        let expr = IndexTarget::Expression("lower(email)");
        assert!(matches!(columns, IndexTarget::Columns(_)));
        assert!(matches!(expr, IndexTarget::Expression(_)));
    }

    #[test]
    fn index_spec_simple_lifts_str_slice_into_column_specs() {
        use super::{IndexKind, IndexSpec, IndexTarget, IndexType};
        let spec = IndexSpec::simple("idx", &["first", "last"], false, IndexType::BTree);
        match spec.target {
            IndexTarget::Columns(cols) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].name, "first");
                assert_eq!(cols[1].name, "last");
                assert_eq!(cols[0].opclass, None);
                assert_eq!(cols[1].opclass, None);
            }
            IndexTarget::Expression(_) => panic!("expected Columns target"),
        }
        assert!(matches!(spec.kind, IndexKind::NonUnique));
        // Column order matters — reversing produces a different index.
        let reverse = IndexSpec::simple("idx", &["last", "first"], false, IndexType::BTree);
        match (spec.target, reverse.target) {
            (IndexTarget::Columns(a), IndexTarget::Columns(b)) => {
                assert_eq!(a[0].name, "first");
                assert_eq!(b[0].name, "last");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn index_spec_simple_maps_unique_to_unique_constraint() {
        use super::{IndexKind, IndexSpec, IndexType};
        let spec = IndexSpec::simple("uix", &["email"], true, IndexType::BTree);
        assert!(matches!(spec.kind, IndexKind::UniqueConstraint));
    }

    #[test]
    fn index_spec_equality_and_clone_preserve_new_fields() {
        use super::{
            IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget,
            IndexType,
        };
        // Construct an `IndexSpec` literal that exercises *every* new
        // Phase 7-Zero field at a non-default value. If `Clone` or
        // `PartialEq` drop any field — or if a future refactor reorders
        // the fields and forgets one — this test catches the regression.
        static COLS: &[IndexColumnSpec] = &[IndexColumnSpec {
            name: "email",
            opclass: Some("text_pattern_ops"),
            order: IndexOrder::Desc,
            nulls: IndexNullsOrder::First,
        }];
        let a = IndexSpec {
            name: "uniq_email_active",
            target: IndexTarget::Columns(COLS),
            kind: IndexKind::UniqueIndex,
            index_type: IndexType::BTree,
            predicate: Some("deleted_at IS NULL"),
            include: &["tenant_id"],
            nulls_not_distinct: true,
            requires_out_of_transaction: true,
            extension_dependency: Some("postgis"),
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(b.predicate, Some("deleted_at IS NULL"));
        assert_eq!(b.include, &["tenant_id"]);
        assert!(b.nulls_not_distinct);
        assert!(matches!(b.kind, IndexKind::UniqueIndex));
        match b.target {
            IndexTarget::Columns(cs) => {
                assert_eq!(cs[0].opclass, Some("text_pattern_ops"));
                assert!(matches!(cs[0].order, IndexOrder::Desc));
                assert!(matches!(cs[0].nulls, IndexNullsOrder::First));
            }
            IndexTarget::Expression(_) => panic!("expected Columns target"),
        }

        // Also round-trip the legacy `simple()` path.
        let c = IndexSpec::simple("idx", &["col"], false, IndexType::BTree);
        let d = c.clone();
        assert_eq!(c, d);
    }

    /// Pin the complete field list of `IndexSpec` — if a future edit
    /// re-introduces a top-level `opclass` field (or drops one of the new
    /// v3 fields), the destructuring pattern below stops matching
    /// exhaustively and the test fails to compile. This is the
    /// machine-checked counterpart to the plan-text rule in §4:
    /// "top-level `IndexSpec::opclass` field is **removed**
    /// (per-column opclass lives on `IndexColumnSpec`)".
    #[test]
    fn index_spec_field_set_is_frozen_to_v3_shape() {
        use super::{IndexSpec, IndexType};
        let spec = IndexSpec::simple("idx", &["col"], false, IndexType::BTree);
        let IndexSpec {
            name: _,
            target: _,
            kind: _,
            index_type: _,
            predicate: _,
            include: _,
            nulls_not_distinct: _,
            requires_out_of_transaction: _,
            extension_dependency: _,
        } = spec;
    }

    // ── T4 (Phase 7-Zero v3) — index_name deterministic helper ──────────────

    #[test]
    fn index_name_short_non_unique_is_verbatim() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        assert_eq!(
            index_name(
                "users",
                IndexNameKind::NonUnique,
                IndexNameTarget::Columns(&["email"])
            ),
            "users_email_idx"
        );
    }

    #[test]
    fn index_name_short_unique_constraint_uses_key_stem() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        assert_eq!(
            index_name(
                "orgs",
                IndexNameKind::UniqueConstraint,
                IndexNameTarget::Columns(&["org_id", "external_id"])
            ),
            "orgs_org_id_external_id_key"
        );
    }

    #[test]
    fn index_name_short_unique_index_uses_uidx_stem() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        assert_eq!(
            index_name(
                "accounts",
                IndexNameKind::UniqueIndex,
                IndexNameTarget::Columns(&["email"])
            ),
            "accounts_email_uidx"
        );
    }

    #[test]
    fn index_name_expression_target_uses_expr_stem() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        assert_eq!(
            index_name(
                "users",
                IndexNameKind::NonUnique,
                IndexNameTarget::Expression
            ),
            "users_expr_idx"
        );
        assert_eq!(
            index_name(
                "users",
                IndexNameKind::UniqueIndex,
                IndexNameTarget::Expression
            ),
            "users_expr_uidx"
        );
    }

    #[test]
    fn index_name_column_order_is_semantic() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        let a = index_name(
            "people",
            IndexNameKind::NonUnique,
            IndexNameTarget::Columns(&["last", "first"]),
        );
        let b = index_name(
            "people",
            IndexNameKind::NonUnique,
            IndexNameTarget::Columns(&["first", "last"]),
        );
        assert_ne!(
            a, b,
            "column order must produce different names byte-for-byte"
        );
        assert_eq!(a, "people_last_first_idx");
        assert_eq!(b, "people_first_last_idx");
    }

    #[test]
    fn index_name_long_input_truncates_to_55_plus_8hex_suffix() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        // Deliberately over-long table + column combination so the
        // naive name exceeds 63 bytes.
        let table = "very_long_table_with_many_underscore_separated_words";
        let cols = ["first_column_name", "second_column_name"];
        let name = index_name(
            table,
            IndexNameKind::NonUnique,
            IndexNameTarget::Columns(&cols),
        );
        assert_eq!(
            name.len(),
            55 + 1 + 8,
            "truncated name layout: 55-byte stem + `_` + 8-char hex digest; got '{name}'"
        );
        // Stem must be an ASCII prefix of the naive full name.
        let naive = format!("{}_{}_{}_{}", table, cols[0], cols[1], "idx");
        assert!(
            naive.as_bytes().starts_with(name.as_bytes()[..55].as_ref()),
            "truncated stem must be a prefix of the pre-truncation full name"
        );
        // The suffix must be 8 hex digits.
        let tail = &name[name.len() - 8..];
        assert!(
            tail.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "hash suffix must be 8 lowercase hex chars; got '{tail}'"
        );
    }

    #[test]
    fn index_name_near_duplicate_long_inputs_do_not_collide() {
        use super::{IndexNameKind, IndexNameTarget, index_name};
        // Two inputs that differ only past the 55th byte of the
        // pre-truncation name — without the hash suffix, both would
        // collide on the same 55-byte prefix.
        let table = "very_long_table_with_many_underscore_separated_words";
        let a = index_name(
            table,
            IndexNameKind::NonUnique,
            IndexNameTarget::Columns(&["payload_one_extra_suffix_a"]),
        );
        let b = index_name(
            table,
            IndexNameKind::NonUnique,
            IndexNameTarget::Columns(&["payload_one_extra_suffix_b"]),
        );
        assert_ne!(a, b, "hash suffix must break near-duplicate collisions");
        assert_eq!(a.len(), 55 + 1 + 8);
        assert_eq!(b.len(), 55 + 1 + 8);
    }

    #[test]
    fn pk_type_desc_variants_resolve_to_id_column() {
        // Constructing a minimal descriptor twice — once per new variant —
        // and asserting `pk_column` answers `Some("id")` is the cleanest
        // way to pin the Phase 7-Zero PkType addition.
        use super::super::relation::{OnDelete, RelationKind};

        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
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
            deferrable: false,
            initially_deferred: false,
            target_type_name: None,
            visage_map: &[],
        }];

        let _ = (OnDelete::Restrict, RelationKind::ForeignKey);

        for pk in [PkType::HeerIdDesc, PkType::RanjIdDesc] {
            let desc = ModelDescriptor {
                type_name: "Desc",
                table_name: "descs",
                pk_type: pk,
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
                app: None,
                moved_from_app: None,
                renamed_from: None,
            };
            assert_eq!(desc.pk_column(), Some("id"));
        }
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
                deferrable: false,
                initially_deferred: false,
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
                deferrable: false,
                initially_deferred: false,
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
            app: None,
            moved_from_app: None,
            renamed_from: None,
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
        deferrable: false,
        initially_deferred: false,
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
        deferrable: false,
        initially_deferred: false,
        target_type_name: None,
        visage_map: &[],
    };

    static T11_BOUNDARY_COLS: &[super::IndexColumnSpec] =
        &[super::IndexColumnSpec::simple("boundary")];
    static T11_LABEL_COLS: &[super::IndexColumnSpec] = &[super::IndexColumnSpec::simple("label")];

    static T11_GIST_INDEX: IndexSpec = IndexSpec {
        name: "idx_boundary_gist",
        target: super::IndexTarget::Columns(T11_BOUNDARY_COLS),
        kind: super::IndexKind::NonUnique,
        index_type: IndexType::Gist,
        predicate: None,
        include: &[],
        nulls_not_distinct: false,
        requires_out_of_transaction: true,
        extension_dependency: Some("postgis"),
    };

    static T11_BTREE_INDEX: IndexSpec = IndexSpec {
        name: "idx_boundary_btree",
        target: super::IndexTarget::Columns(T11_BOUNDARY_COLS),
        kind: super::IndexKind::NonUnique,
        index_type: IndexType::BTree,
        predicate: None,
        include: &[],
        nulls_not_distinct: false,
        requires_out_of_transaction: false,
        extension_dependency: None,
    };

    static T11_GIST_ON_TEXT: IndexSpec = IndexSpec {
        name: "idx_label_gist",
        target: super::IndexTarget::Columns(T11_LABEL_COLS),
        kind: super::IndexKind::NonUnique,
        index_type: IndexType::Gist,
        predicate: None,
        include: &[],
        nulls_not_distinct: false,
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
            app: None,
            moved_from_app: None,
            renamed_from: None,
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
            app: None,
            moved_from_app: None,
            renamed_from: None,
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
            app: None,
            moved_from_app: None,
            renamed_from: None,
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
            app: None,
            moved_from_app: None,
            renamed_from: None,
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
/// Field order: `id` (omitted for `pk = None`), then `created_at`,
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
    /// `#[field(deferrable)]` — only meaningful for relation fields.
    pub deferrable: bool,
    /// `#[field(initially_deferred)]` — only meaningful when
    /// `deferrable` is true.
    pub initially_deferred: bool,

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

/// Describes an adopter-declared custom PK type.
///
/// Named struct (not bare enum-variant fields) so future fields can be
/// added without rewriting every `PkType::Custom` match arm in the
/// codebase. Added in Phase 7-Zero-2 T1 alongside the
/// [`PrimaryKey`](crate::primary_key::PrimaryKey) trait substrate; Task 3
/// wires the attribute-parse + macro emission path that populates the
/// fields from `#[model(pk = MyCustomId)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CustomPrimaryKeyKind {
    /// Fully-qualified type name of the custom PK — e.g. `"crate::ids::UserId"`.
    pub type_name: &'static str,
    /// Postgres column type emitted into DDL — e.g. `"UUID"` / `"BIGINT"`.
    pub sql_type: &'static str,
    /// Column `DEFAULT` SQL — e.g. `"gen_random_uuid()"`. Empty string
    /// denotes no default (client-generated).
    pub default_sql: &'static str,
}

/// Primary key strategy.
///
/// The six leaf variants (`HeerId`, `RanjId`, `HeerIdDesc`, `RanjIdDesc`,
/// `Serial`, `None`) map 1:1 to the `#[model(pk = X)]` attribute identifiers
/// (`HeerId`, `RanjId`, `HeerIdRecencyBiased` | `HeerIdDesc`,
/// `RanjIdRecencyBiased` | `RanjIdDesc`, `Serial`, `None`).
/// `Composite` is emitted for models that declare multiple PK columns —
/// rare, mostly join tables — and carries the ordered list of column names.
/// `Custom` is emitted for adopter-declared PK types registered through
/// [`PrimaryKey`](crate::primary_key::PrimaryKey) + `djogi::primary_key!`
/// (wiring lands in Task 3; the variant exists from T1 so match sites
/// across the crate freeze their exhaustiveness contract now).
///
/// `HeerIdDesc` / `RanjIdDesc` (added in Phase 7-Zero v3) store the same
/// logical identity as their ascending siblings but with timestamp + sequence
/// bits XORed so that a BTree index on the PK column scans
/// **most-recent-first** without a secondary descending index. See
/// [`crate::types::HeerIdDesc`] / [`crate::types::RanjIdDesc`] and the
/// Phase 7-Zero plan §4.1 for the full indexing trade-off. The ascending ↔
/// descending PK migration itself lands in Phase 7; 7-Zero only freezes the
/// variant additions, attribute-parse paths, and descriptor shape.
///
/// `#[non_exhaustive]` guards the enum so future PK shapes (sharded IDs,
/// app-scoped IDs, etc.) can be added without breaking downstream match
/// sites — callers must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PkType {
    HeerId,
    RanjId,
    HeerIdDesc,
    RanjIdDesc,
    Serial,
    None,
    Composite(&'static [&'static str]),
    Custom(CustomPrimaryKeyKind),
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
///
/// `Clone` was added in Phase 7 T2 because the migrate module's
/// differ-test fixtures need to construct multiple variants of a
/// descriptor via struct-update syntax (`..base.clone()`); the
/// derived implementation does a deep copy of the `Option<PartitionSpec>`
/// and `Option<FtsDescriptor>` fields and a shallow copy of the
/// `&'static` references.
#[derive(Debug, Clone)]
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

    // ── Apps subsystem (Phase 7-Zero v3 T8) ─────────────────────────────────
    /// The stable string label of the app this model belongs to.
    ///
    /// Set by `#[model(app = Vehicles)]` — the macro lowers the type
    /// path to `<Vehicles as ::djogi::App>::LABEL` at const-eval time
    /// so the descriptor carries the Postgres identifier, not the
    /// Rust type name. `None` places the model in the synthetic
    /// global bucket, which Phase 7's differ files under
    /// `<default-database>/<empty-label>/`.
    pub app: Option<&'static str>,

    /// Historical-metadata pointer to this model's prior app.
    ///
    /// Set by `#[model(moved_from_app = OldBilling)]` when a model
    /// has been moved between apps. Enables Phase 7's migration
    /// differ to emit correct move-table-across-schemas operations
    /// without forcing the old app to stay declared. The pointed-at
    /// app may be tombstoned — that's the expected retirement flow
    /// (see `docs/guide/apps.md`).
    pub moved_from_app: Option<&'static str>,

    /// Prior table name when the model has been renamed via
    /// `#[model(table = "...", renamed_from = "old_table")]`. Phase
    /// 7's migration differ uses this to emit
    /// `ALTER TABLE old_table RENAME TO new_table` rather than a
    /// destructive DROP+CREATE pair.
    ///
    /// Carries the old **string** table name, not a type — old
    /// model types may no longer exist in source after a sweep that
    /// renamed many tables in one pass.
    ///
    /// `None` for every model that has not been renamed (the common
    /// case).
    pub renamed_from: Option<&'static str>,
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
    /// 2. Skip entries whose `target` is [`IndexTarget::Expression`] —
    ///    expression-target spatial indexes are legal but their column
    ///    relationship is opaque here, so we conservatively answer "no"
    ///    rather than guess.
    /// 3. For each [`IndexColumnSpec`] in the spec's column list, check
    ///    whether the corresponding [`FieldDescriptor`] has
    ///    `sql_type == FieldSqlType::Geography { .. }`.
    /// 4. Return `true` as soon as one such matching field is found.
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
            let cols = match idx.target {
                IndexTarget::Columns(cs) => cs,
                IndexTarget::Expression(_) => continue,
            };
            for col in cols {
                let is_geo = self
                    .fields
                    .iter()
                    .find(|f| f.name == col.name)
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
    /// Returns `Some("id")` for the five standard PK types (`HeerId`,
    /// `RanjId`, `HeerIdDesc`, `RanjIdDesc`, `Serial`) and `None` for
    /// `pk = None` models. `Composite` PKs are uncommon; this method
    /// returns the first column in the composite list on the assumption
    /// that it is the most natural tiebreak candidate.
    ///
    /// Used by [`crate::query::order::OrderExpr::spatial_distance_with_pk_tiebreak`]
    /// to capture the PK column at `order_by_distance` construction time.
    pub fn pk_column(&self) -> Option<&'static str> {
        match &self.pk_type {
            PkType::HeerId
            | PkType::RanjId
            | PkType::HeerIdDesc
            | PkType::RanjIdDesc
            | PkType::Serial => Some("id"),
            PkType::None => None,
            PkType::Composite(cols) => cols.first().copied(),
            // Custom-PK models inject an `id` column the same way the
            // built-in variants do; the custom payload is what names
            // the Rust type, not the column.
            PkType::Custom(_) => Some("id"),
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

    use super::{FieldSqlType, IndexKind, IndexTarget, IndexType, ModelDescriptor};

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
    ///
    /// This is a simplified projection of the Phase 7-Zero `IndexSpec`
    /// tailored to the Phase 6 `MigrationShape` contract — Phase 7's real
    /// differ consumes the full `IndexSpec` directly.
    #[derive(Debug, Clone)]
    pub struct IndexShape {
        /// Index name from `IndexSpec::name`.
        pub name: &'static str,
        /// Column names extracted from the underlying
        /// `IndexSpec::target`. For `IndexTarget::Columns`, this is the
        /// per-column `name` field from every `IndexColumnSpec`. For
        /// `IndexTarget::Expression`, this is an empty vector and the
        /// expression text lives inside `sql_text` instead.
        pub columns: Vec<&'static str>,
        /// `true` when the underlying `IndexSpec::kind` is either
        /// `IndexKind::UniqueConstraint` or `IndexKind::UniqueIndex`. The
        /// MigrationShape contract does not distinguish the two forms —
        /// Phase 7's real differ reads `IndexSpec::kind` directly.
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
                    // Phase 7-Zero v3: IndexSpec now carries
                    // `target: IndexTarget` (column list | expression) and
                    // `kind: IndexKind`. Collapse back to the
                    // `(columns: Vec<&str>, unique: bool)` shape the Phase 6
                    // MigrationShape contract expects — the richer per-column
                    // knobs (opclass / order / nulls) stay on the underlying
                    // IndexSpec for Phase 7's real differ to consume.
                    let (columns, target_sql) = match spec.target {
                        IndexTarget::Columns(cs) => {
                            let names: Vec<&'static str> = cs.iter().map(|c| c.name).collect();
                            let joined = names.join(",");
                            (names, joined)
                        }
                        IndexTarget::Expression(expr) => (Vec::new(), expr.to_string()),
                    };

                    let is_unique = matches!(
                        spec.kind,
                        IndexKind::UniqueConstraint | IndexKind::UniqueIndex
                    );
                    let create_kw = if is_unique {
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
                        "{create_kw}{concurrently} IF NOT EXISTS {name} ON {table} USING {type_kw}({target_sql})",
                        name = spec.name,
                    );

                    IndexShape {
                        name: spec.name,
                        columns,
                        unique: is_unique,
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
