//! Runtime model descriptors — emitted by `#[model]` via `inventory`,
//! consumed by the migration system, `djogi docs`,
//! RLS generation, and the partitioning analyzer .
//! # Why `ModelDescriptor` is the single source of truth
//! The framework has several consumers that need to reason about a model's
//! schema at runtime or build time: the migration differ compares the live
//! set of `ModelDescriptor`s against `schema_snapshot.json`; the admin UI
//! renders columns from them; `djogi docs` turns them into markdown; the
//! RLS generator produces `CREATE POLICY` statements from `tenant_key`.
//! Rather than each subsystem re-parsing `#[model]` attributes, the proc
//! macro emits one `inventory::submit!(ModelDescriptor { ... })` per model
//! and every consumer iterates `inventory::iter::<ModelDescriptor>`.
//! # Why the descriptor has forward-declared fields
//! Some fields here (`partition_by`, `has_outbox`, `tenant_key`, `cache_ttl`,
//! `rationale`, `indexes`, plus per-field `outbox_exclude`, `index_type`,
//! `rationale`) are declared in but *populated* by later phases. They
//! default to `None` / `false` / `&[]` in so the struct layout is
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
// The migration differ, which reads both relation and FTS metadata from
// the descriptor module path.
pub use crate::fts::FtsDescriptor;

/// Subtype discriminator for `FieldSqlType::Geography`.
/// The migration differ compares subtypes by discriminant, not by
/// `Display` text, so subtype renames or new variants do not surface as
/// spurious migration diffs.
/// Sealed via `#[non_exhaustive]` — adding a variant in a future phase is
/// not a breaking change for downstream consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeographySubtype {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl std::fmt::Display for GeographySubtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Point => "Point",
            Self::LineString => "LineString",
            Self::Polygon => "Polygon",
            Self::MultiPoint => "MultiPoint",
            Self::MultiLineString => "MultiLineString",
            Self::MultiPolygon => "MultiPolygon",
        })
    }
}

/// SQL type a model field maps to.
/// This enum is the bridge between Rust field types and the column types
/// the migration system generates. The proc macro maps `String -> Text`,
/// `i64 -> BigInt`, `bool -> Boolean`, etc. User code rarely constructs
/// these directly — they appear in emitted `FieldDescriptor` literals.
/// `Custom` exists for scalar types the framework doesn't model natively
/// (e.g. extension types like `"ltree"`, or schema-qualified domain names
/// like `"public.positive_amount"`). For simple unqualified Postgres domains,
/// `#[field(domain = "...")]` emits the dedicated [`Domain`] variant
/// instead. The migration differ treats `Custom("FOO")` and `Custom("FOO")`
/// as equal (string compare), so adding support for a new type is
/// non-breaking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSqlType {
    Text,
    /// `VARCHAR(n)` — bounded character storage. Emitted for `String`
    /// fields annotated with `#[field(max_length = N)]`. The differ
    /// treats `Text` ↔ `Varchar(n)` and `Varchar(m)` ↔ `Varchar(n)` as
    /// type-change drift requiring an
    /// `ALTER TABLE … ALTER COLUMN … TYPE` migration, exactly like any
    /// other column-type change.
    /// `n` must be in `1..=10_485_760`. Values outside this range are
    /// rejected at macro-expansion time with a compile error. Postgres
    /// internally caps `VARCHAR` at 10,485,760 characters (equal to its
    /// maximum row size); values above that are impractical.
    Varchar(u32),
    SmallInt,
    Integer,
    BigInt,
    Real,
    DoublePrecision,
    Boolean,
    Timestamptz,
    Date,
    /// Bare `NUMERIC` — unbounded precision/scale. Used for two distinct
    /// adopter source types, disambiguated at projection time by the
    /// `FieldDescriptor::rust_source_type` discriminator (`RustSourceType`):
    /// * `Some(RustSourceType::U64)` — `u64` columns, with a range +
    ///   integrality CHECK (`col >= 0 AND col <= u64::MAX AND col = trunc(col)`).
    /// * `Some(RustSourceType::Decimal)` — `rust_decimal::Decimal` columns,
    ///   with a structural CHECK enforcing the 96-bit mantissa / scale ≤ 28
    ///   representable range (`djogi#188`).
    /// * `None` — adopter custom types whose `DjogiSqlType::SQL_TYPE`
    ///   resolves to `"NUMERIC"`; no Rust-derived CHECK is projected because
    ///   the framework cannot know an adopter scalar type's representable
    ///   range.
    ///   The bare variant preserves the adopter's full precision and scale
    ///   verbatim — Postgres does not round inputs before the CHECK fires.
    Numeric,
    /// `NUMERIC(precision, scale)` — bounded numeric. Not currently emitted
    /// by the framework for any built-in Rust source type: `u64` historically
    /// used `NUMERIC(20, 0)` but is now `Numeric` + CHECK, and
    /// `rust_decimal::Decimal` ships as `Numeric` + structural
    /// CHECK rather than a bounded `NUMERIC(P, S)` shape (see the
    /// `Decimal precision and scale projection` decision row
    /// for why every fixed `(P, S)` default either rejects realistic adopter
    /// values or silently rounds them). The variant is retained so adopter
    /// custom types (`DjogiSqlType::SQL_TYPE = "NUMERIC(P, S)"`) and any
    /// future bounded-NUMERIC use cases reach the differ without a fresh
    /// variant. The differ compares variants structurally (precision and
    /// scale are part of `PartialEq`), so a precision change emits a
    /// `ColumnChange::ChangeType` like any other type evolution.
    NumericPrecision {
        precision: u8,
        scale: u8,
    },
    Uuid,
    Jsonb,
    /// Postgres `BYTEA` — raw variable-length binary data.
    /// Rust source type is `Vec<u8>` (nullable form `Option<Vec<u8>>`).
    /// `tokio-postgres` ships the native `ToSql`/`FromSql` wire codec for
    /// `Vec<u8>` ↔ BYTEA, so no third-party crate is pulled in and the
    /// `FromPgRow` codegen uses the same `row.try_get::<_, Vec<u8>>(...)`
    /// path as every other built-in scalar.
    /// Always-available — no feature flag. Distinct from `SMALLINT` (the
    /// lowering for a *scalar* `u8` field): the `Vec<u8>` byte-vector shape
    /// is recognised ahead of the generic `Vec<T>` array arms in
    /// `rust_type_to_sql`, so a `Vec<u8>` is treated as raw bytes — not as a
    /// one-dimensional `SMALLINT[]`.
    Bytea,
    TextArray,
    IntegerArray,
    BigIntArray,
    BoolArray,
    /// `SMALLINT[]` — one-dimensional array of 16-bit integers.
    SmallIntArray,
    /// `REAL[]` — one-dimensional array of single-precision floats.
    RealArray,
    /// `DOUBLE PRECISION[]` — one-dimensional array of double-precision floats.
    DoublePrecisionArray,
    /// `TIMESTAMPTZ[]` — one-dimensional array of timezone-aware timestamps.
    TimestamptzArray,
    /// `DATE[]` — one-dimensional array of calendar dates.
    DateArray,
    /// `UUID[]` — one-dimensional array of UUIDs. Also used for `Vec<RanjId>`
    /// columns, since `RanjId` is encoded as `UUID` on the wire.
    UuidArray,
    /// `NUMERIC[]` — one-dimensional array of arbitrary-precision decimals.
    NumericArray,
    /// Case-insensitive text (Postgres `CITEXT`). Declared in ;
    /// used by the SQL linting plan in later phases.
    Citext,
    /// PostGIS `geography(<subtype>, SRID)`. The `subtype` discriminant
    /// is a typed `GeographySubtype` so the migration differ can
    /// compare subtypes by discriminant — subtype renames or new variants
    /// do not surface as spurious migration diffs.
    /// Shipped with `Point` hardcoded in `Display`; the descriptor
    /// shape is frozen for the consumer.
    Geography {
        subtype: GeographySubtype,
        srid: u32,
    },
    /// Postgres `INTERVAL`. Rust source type is
    /// `djogi::Interval` (`pg_types::Interval`) — a three-component
    /// newtype mirroring the Postgres wire format `(microseconds, days,
    /// months)`. The `time` crate's `Duration` is intentionally NOT
    /// supported here: a `Duration` is a fixed-microsecond duration
    /// with no calendar-component story, and collapsing the three
    /// Postgres fields into one would silently corrupt DST-straddling
    /// or month-arithmetic results. See `djogi::Interval` docs for the
    /// full rationale.
    /// Always-available — no feature flag. The codec lives in
    /// `pg_types.rs` and pulls in no third-party crate.
    Interval,
    /// Postgres `INET` — IP address column type (, `network`
    /// feature). Rust source type is [`std::net::IpAddr`]; the wire
    /// codec is `postgres-types`' always-on native impl, which writes
    /// the netmask as /32 (IPv4) or /128 (IPv6) — host-address case.
    /// The descriptor variant is unconditional so the migration
    /// differ / docs / `Display` surface stays stable across feature
    /// states; only the Rust-side type recognition is gated behind the
    /// `network` feature. Adopters with the feature off who write
    /// `pub host: std::net::IpAddr` get an "unresolved type" compile
    /// error at the model struct via the missing `IntoFilterValue` /
    /// `DjogiPortableEq` impls, not a macro-time error — matching the
    /// spatial flag pattern.
    Inet,
    /// Postgres `CIDR` — network address column type (,
    /// `network` feature). Rust source type is `djogi::CidrAddr`
    /// (`pg_types::CidrAddr`) — a typed newtype carrying both
    /// address and prefix length, with construction-time validation
    /// that host bits past the prefix are zero (Postgres's CIDR
    /// admission rule).
    /// Distinct from [`Inet`](FieldSqlType::Inet) by Rust type rather
    /// than by attribute: `IpAddr` → INET, `CidrAddr` → CIDR. The
    /// macro routes between them without needing a `#[field(ip_type
    /// = "...")]` attribute.
    Cidr,
    /// Postgres `MACADDR` — EUI-48 MAC address column type
    /// (, `network` feature). Rust source type is
    /// `djogi::MacAddr` (`pg_types::MacAddr`) — a typed newtype over
    /// `[u8; 6]` with a hand-rolled 6-byte wire codec.
    /// `MACADDR8` (the 8-byte EUI-64 variant) is intentionally out of
    /// scope for the surface; the tracks future-work
    /// for EUI-64 if adopter demand surfaces.
    Macaddr,
    /// Postgres range type.
    /// Rust source type is `djogi::Range<T>` (`pg_types::Range<T>`).
    /// The `subtype` discriminant names the Postgres range type the
    /// column resolves to:
    /// * [`RangeSubtypeKind::Int4`] — `int4range` (Rust: `Range<i32>`)
    /// * [`RangeSubtypeKind::Int8`] — `int8range` (Rust: `Range<i64>`)
    /// * [`RangeSubtypeKind::Num`] — `numrange`
    ///   (Rust: `Range<rust_decimal::Decimal>`)
    /// * [`RangeSubtypeKind::Ts`] — `tsrange`
    ///   (Rust: `Range<time::PrimitiveDateTime>`)
    /// * [`RangeSubtypeKind::Tstz`] — `tstzrange`
    ///   (Rust: `Range<djogi::DateTime>`)
    /// * [`RangeSubtypeKind::Date`] — `daterange`
    ///   (Rust: `Range<djogi::Date>`)
    ///   `tsrange` is the single timestamp-without-timezone range entry
    ///   point and uses `time::PrimitiveDateTime`. Djogi's `DateTime`
    ///   alias remains timezone-aware and lowers to `tstzrange`.
    /// # Future siblings
    /// This variant is the shared substrate for two future DB-level
    /// no-overlap lanes; neither is shipped by #215:
    /// * **** — `btree_gist` EXCLUDE constraint grammar
    ///   (`#[model(exclude(...))]`) and `CREATE EXTENSION btree_gist`.
    /// * **** — PostgreSQL 18 temporal-constraint DDL
    ///   (`WITHOUT OVERLAPS`, `PERIOD` foreign keys, `NOT ENFORCED`,
    ///   named `NOT NULL`).
    ///   Both lanes consume range columns as input but neither alters
    ///   the [`FieldSqlType::Range`] variant itself.
    Range {
        /// The Postgres range subtype this column resolves to.
        subtype: RangeSubtypeKind,
    },
    /// Postgres domain reference — `#[field(domain = "<name>")]` (
    /// Piece A). Names an adopter-managed Postgres `CREATE DOMAIN <name>`
    /// type that already exists in the target database. Emitting the
    /// `CREATE DOMAIN` DDL from djogi (Piece B, `#[model(domains = [...])]`)
    /// is deferred to a follow-on lane; Piece A only references the
    /// adopter-supplied domain.
    /// # Fields
    /// * `name` — the domain identifier. Validated at macro-parse time
    ///   against the standard Postgres unquoted-identifier rules
    ///   (`check_domain_name` in `djogi-macros::ident`): non-empty,
    ///   ≤63 bytes, ASCII letter or underscore first, ASCII alphanumeric
    ///   or underscore after. Reserved-keyword and `__djogi_` prefix
    ///   checks are intentionally NOT applied — domain identifiers are
    ///   SQL type names, not column / table identifiers, and
    ///   `domain = "text"` is a legitimate (if confusing) declaration.
    ///   Schema-qualified domain names (`"public.positive_amount"`) fail
    ///   the dotless-identifier rule and are out of Piece A scope
    ///   adopters needing them fall back to `Custom("public.positive_amount")`
    ///   until Piece B.
    /// * `base` — the inferred underlying [`FieldSqlType`] for the Rust
    ///   field's source type, captured for documentation and future
    ///   Piece B consumption. **Piece A treats `base` as purely
    ///   informational**: the migration differ, projection, composer, and
    ///   snapshot all key off the rendered `Display` output (the domain
    ///   name) rather than the inner base. A future Piece B that emits
    ///   `CREATE DOMAIN <name> AS <base>` will read this slot directly.
    /// # Display contract
    /// [`Display`](#impl-Display-for-FieldSqlType) renders the bare
    /// domain name (e.g. `"positive_amount"`), not the inner base type.
    /// The migration differ compares column types by rendered string,
    /// so a domain-name change surfaces as
    /// [`ColumnChange::ChangeType`](crate::migrate::ColumnChange::ChangeType)
    /// with `from = "old_name"`, `to = "new_name"` — the same shape as
    /// any other type evolution.
    /// # Why `&'static FieldSqlType`, not `Box<FieldSqlType>`
    /// The recursive shape (`Domain` carrying a nested `FieldSqlType`)
    /// must coexist with the existing `pub const fn field_descriptor`
    /// constructor and every static / const `FieldDescriptor` literal
    /// the test suite and downstream consumers rely on. `Box<...>` has
    /// a non-trivial destructor that const evaluation cannot run, so
    /// even when the variant in question is `Text` or `BigInt` the
    /// enum's auto-generated `Drop` would propagate and reject every
    /// `const fn field_descriptor(...)` call site.
    /// `&'static FieldSqlType` resolves this cleanly:
    /// * **No Drop.** The enum stays trivially droppable; `const fn`
    ///   construction continues to work.
    /// * **Idiomatic.** Every other nested descriptor structure on the
    ///   surface already uses `&'static [...]`
    ///   [`FieldDescriptor::visage_map`], [`ModelDescriptor::fields`],
    ///   [`IndexTarget::Columns`]. `Box` would be the outlier here.
    /// * **Zero heap allocation.** Descriptors live for the entire
    ///   process lifetime (registered via `inventory::submit!`); a
    ///   `Box<FieldSqlType>` would allocate once per domain field and
    ///   never be reclaimed. A `&'static` reference into static storage
    ///   has identical lifetime semantics without the allocation.
    /// * **Macro can emit it.** The proc macro places a `static`
    ///   declaration for the base type inside the `inventory::submit!`
    ///   block before the `FieldDescriptor` literal, then references
    ///   it as `&<STATIC>`.
    ///   Test fixtures construct `Domain` values via a one-line `static`
    ///   binding: `static BASE: FieldSqlType = FieldSqlType::Numeric;`
    ///   then `FieldSqlType::Domain { name: "x", base: &BASE }`.
    Domain {
        /// Domain identifier as it appears in `CREATE DOMAIN <name> ...`
        /// validated by the macro to satisfy the Postgres unquoted-
        /// identifier rules.
        name: &'static str,
        /// Inferred underlying [`FieldSqlType`] for the Rust field's
        /// source type. Informational in Piece A; consumed by Piece B
        /// once `CREATE DOMAIN` emission lands. Stored as `&'static`
        /// rather than `Box` so the enclosing enum stays trivially
        /// droppable and `const fn field_descriptor(...)` calls continue
        /// to compile in const contexts.
        base: &'static FieldSqlType,
    },
    /// Fallback for SQL types the framework doesn't model explicitly.
    /// Stored verbatim and compared by string equality in the migration differ.
    Custom(&'static str),
}

/// Postgres range-subtype discriminant for [`FieldSqlType::Range`].
/// Mirrors `pg_types::RangeSubtype` (the Rust-element-to-Postgres-range
/// mapping carried by the wire codec): there is one variant per
/// supported Postgres built-in range type. The two surfaces are
/// intentionally separate — `RangeSubtype` lives on the Rust element
/// type (open trait, adopter-extensible if a custom element type ever
/// needs it); `RangeSubtypeKind` lives on the descriptor (closed enum,
/// reachable by the migration differ and `Display`).
/// # Wire-format SQL identifier (`Display`)
/// `Display` renders the lowercase Postgres range type name
/// (`int4range`, `int8range`, `numrange`, `tsrange`, `tstzrange`,
/// `daterange`).
/// The schema snapshot stores the rendered form because the differ
/// compares column types by rendered string, not by enum identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeSubtypeKind {
    /// `int4range` — `Range<i32>` columns.
    Int4,
    /// `int8range` — `Range<i64>` columns.
    Int8,
    /// `numrange` — `Range<rust_decimal::Decimal>` columns.
    Num,
    /// `tsrange` — `Range<time::PrimitiveDateTime>` columns.
    Ts,
    /// `tstzrange` — `Range<DateTime>` columns (timezone-aware).
    Tstz,
    /// `daterange` — `Range<Date>` columns.
    Date,
}

impl std::fmt::Display for RangeSubtypeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeSubtypeKind::Int4 => write!(f, "int4range"),
            RangeSubtypeKind::Int8 => write!(f, "int8range"),
            RangeSubtypeKind::Num => write!(f, "numrange"),
            RangeSubtypeKind::Ts => write!(f, "tsrange"),
            RangeSubtypeKind::Tstz => write!(f, "tstzrange"),
            RangeSubtypeKind::Date => write!(f, "daterange"),
        }
    }
}

/// Schema-type bridge for user-defined scalar wrappers.
/// The model macro maps built-in Rust field types directly to
/// [`FieldSqlType`] variants. For adopter-defined scalar types whose SQL type
/// is declared by another Djogi macro, such as [`crate::primary_key!`] custom
/// IDs or [`crate::DjogiEnum`] enums, the macro falls back to this associated
/// constant and emits [`FieldSqlType::Custom`].
pub trait DjogiSqlType {
    const SQL_TYPE: &'static str;
}

impl std::fmt::Display for FieldSqlType {
    /// Used by `djogi docs` — produces clean SQL type names (`TEXT`, not
    /// `Text` or `Custom("BYTEA")`). Never change this to forward to
    /// `Debug`: the generated docs would become unreadable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldSqlType::Text => write!(f, "TEXT"),
            FieldSqlType::Varchar(n) => write!(f, "VARCHAR({n})"),
            FieldSqlType::SmallInt => write!(f, "SMALLINT"),
            FieldSqlType::Integer => write!(f, "INTEGER"),
            FieldSqlType::BigInt => write!(f, "BIGINT"),
            FieldSqlType::Real => write!(f, "REAL"),
            FieldSqlType::DoublePrecision => write!(f, "DOUBLE PRECISION"),
            FieldSqlType::Boolean => write!(f, "BOOLEAN"),
            FieldSqlType::Timestamptz => write!(f, "TIMESTAMPTZ"),
            FieldSqlType::Date => write!(f, "DATE"),
            FieldSqlType::Numeric => write!(f, "NUMERIC"),
            FieldSqlType::NumericPrecision { precision, scale } => {
                write!(f, "NUMERIC({precision}, {scale})")
            }
            FieldSqlType::Uuid => write!(f, "UUID"),
            FieldSqlType::Jsonb => write!(f, "JSONB"),
            // BYTEA raw binary column. The wire codec is
            // tokio-postgres' native `Vec<u8>` impl; the SQL type string is
            // the bare Postgres `BYTEA` keyword.
            FieldSqlType::Bytea => write!(f, "BYTEA"),
            FieldSqlType::TextArray => write!(f, "TEXT[]"),
            FieldSqlType::IntegerArray => write!(f, "INTEGER[]"),
            FieldSqlType::BigIntArray => write!(f, "BIGINT[]"),
            FieldSqlType::BoolArray => write!(f, "BOOLEAN[]"),
            FieldSqlType::SmallIntArray => write!(f, "SMALLINT[]"),
            FieldSqlType::RealArray => write!(f, "REAL[]"),
            FieldSqlType::DoublePrecisionArray => write!(f, "DOUBLE PRECISION[]"),
            FieldSqlType::TimestamptzArray => write!(f, "TIMESTAMPTZ[]"),
            FieldSqlType::DateArray => write!(f, "DATE[]"),
            FieldSqlType::UuidArray => write!(f, "UUID[]"),
            FieldSqlType::NumericArray => write!(f, "NUMERIC[]"),
            FieldSqlType::Citext => write!(f, "CITEXT"),
            FieldSqlType::Geography { subtype, srid } => {
                write!(f, "geography({subtype}, {srid})")
            }
            // INTERVAL column type. The wire codec carries
            // the three-component encoding; the SQL type string is the
            // bare Postgres `INTERVAL` keyword.
            FieldSqlType::Interval => write!(f, "INTERVAL"),
            // Postgres network family (`network` feature).
            // `Display` always renders the bare SQL keyword regardless
            // of feature state so migration snapshots and docs stay
            // stable when the feature is toggled.
            FieldSqlType::Inet => write!(f, "INET"),
            FieldSqlType::Cidr => write!(f, "CIDR"),
            FieldSqlType::Macaddr => write!(f, "MACADDR"),
            // Postgres range type. `Display` renders the
            // lowercase subtype name (`int4range`, `tsrange`,
            // `tstzrange`, ...); the schema snapshot stores
            // the rendered form so the differ compares by string.
            FieldSqlType::Range { subtype } => write!(f, "{subtype}"),
            // Djogi#216 Piece A — Postgres domain reference.
            // `Display` renders the bare domain name; the inner `base`
            // is informational and never emitted into the column-type
            // slot. The migration differ keys off the rendered string,
            // so a domain rename surfaces as `ColumnChange::ChangeType`
            // with `from = "old_name"`, `to = "new_name"`.
            FieldSqlType::Domain { name, .. } => write!(f, "{name}"),
            FieldSqlType::Custom(s) => write!(f, "{s}"),
        }
    }
}

/// Partition strategy for a model table.
/// Set via `#[model(partition_by = "range:created_at")]` or
/// `#[model(partition_by = "hash:id:8")]`. declares the enum so
/// `ModelDescriptor::partition_by` has a stable type; attribute parsing and
/// partition-aware `QuerySet` land in (partitioning plan).
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
/// Declares; the migration differ emits `USING btree|gin|gist|...`
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

/// Uniqueness discipline for an [`IndexSpec`] — introduced in
/// v3 to replace the binary `unique: bool` field with a three-valued enum.
/// Postgres distinguishes between a `UNIQUE` constraint (declared on the
/// table; created with a backing unique index; references the index by name)
/// and a `UNIQUE INDEX` (a plain index that happens to enforce uniqueness
/// without participating in the constraint catalogue). The former is what
/// most users mean when they say "unique"; the latter is what you reach for
/// when you need `WHERE ... IS NOT NULL` partial uniqueness or
/// `NULLS NOT DISTINCT` semantics that the constraint form does not expose.
/// Variant map:
/// - [`IndexKind::NonUnique`] — a plain index. The typical case.
/// - [`IndexKind::UniqueConstraint`] — `UNIQUE` constraint on the table.
///   `IndexSpec::simple(..., unique = true, ...)` maps to this variant.
/// - [`IndexKind::UniqueIndex`] — `CREATE UNIQUE INDEX` without a constraint
///   row. Required when [`IndexSpec::predicate`] is set or when
///   [`IndexSpec::nulls_not_distinct`] is `true`.
/// # Invariant — unique indexes are btree-only
/// Both unique-bearing variants ([`IndexKind::UniqueConstraint`] and
/// [`IndexKind::UniqueIndex`]) require [`IndexSpec::index_type`] to be
/// [`IndexType::BTree`]. PostgreSQL rejects `CREATE UNIQUE INDEX … USING
/// <non-btree>` server-side, and `ALTER TABLE … ADD CONSTRAINT … UNIQUE`
/// has no `USING` clause at all (it is implicitly btree). The
/// `#[model(indexes(unique(...)))]` macro layer enforces this invariant by
/// rejecting `unique(..., using = "<non-btree>")` at compile time
/// (#83). The migration SQL emitter and projection layer treat
/// the invariant as a precondition: violating it would produce
/// `CREATE UNIQUE INDEX … USING gist` etc., which fails at apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    NonUnique,
    UniqueConstraint,
    UniqueIndex,
}

/// Sort direction for a single column inside an [`IndexColumnSpec`].
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
/// Postgres indexes carry per-column sort direction, per-column nulls
/// ordering, and per-column opclass — flattening these onto the enclosing
/// [`IndexSpec`] (as an earlier v3 draft did) would make multi-column
/// indexes with mixed direction or mixed opclass impossible to express
/// without breaking the descriptor contract later. Keeping them on the
/// column spec leaves the contract additively-extensible for 0.1.0 and
/// beyond.
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
    /// ```ignore
    /// IndexTarget::Columns(&[
    /// IndexColumnSpec::simple("first"),
    /// IndexColumnSpec::simple("last"),
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
/// `ModelDescriptor::indexes` is `&[IndexSpec]` so a model can declare
/// composite / non-BTree indexes that the migration differ turns into
/// `CREATE INDEX` statements. An empty slice — the default — means
/// "only the implicit PK and per-field `#[field(index)]` indexes".
/// # shape
/// Widened the contract from a `(columns, unique)` pair into
/// a richer structure that can express the full Postgres index surface
/// without further breaking changes:
/// - `target` replaces `columns` and uses [`IndexTarget`] to pick either a
///   per-column list ([`IndexTarget::Columns`]) or an expression
///   ([`IndexTarget::Expression`]).
/// - `kind` replaces `unique: bool` with [`IndexKind`] so partial / nulls-
///   not-distinct unique indexes stop being forced through the constraint
///   form.
/// - `predicate`, `include`, and `nulls_not_distinct` are new optional
///   fields matching the Postgres DDL vocabulary.
/// - `requires_out_of_transaction` and `extension_dependency` from
///   are preserved unchanged.
///   Use [`IndexSpec::simple`] to construct a plain column-list index without
///   listing every optional field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    pub name: &'static str,
    pub target: IndexTarget,
    pub kind: IndexKind,
    pub index_type: IndexType,
    /// Partial-index `WHERE` clause, e.g. `"deleted_at IS NULL"`. Raw SQL
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

// ── EXCLUSION constraint metadata (PR 7) ────────────────────────

/// One element of an `EXCLUDE` constraint — an expression and the
/// operator used to compare it against other rows' values.
/// Each element corresponds to one entry in the `EXCLUDE USING method
/// (expr WITH operator [, expr WITH operator …])` clause. Order is
/// significant: it matches the textual source order, which is also the
/// order Postgres records in `pg_constraint`. The migration differ uses
/// positional comparison when classifying changes.
/// # Examples
/// `EXCLUDE USING gist (room_id WITH =, period WITH &&)` decomposes into:
/// ```ignore
/// ExclusionElement { expr: "room_id", with_operator: "=" }
/// ExclusionElement { expr: "period", with_operator: "&&" }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExclusionElement {
    /// Column name or expression. Plain column references and arbitrary
    /// expressions (e.g. `tstzrange(starts_at, ends_at)`) are both
    /// emitted verbatim — the descriptor does not validate or rewrite
    /// the SQL.
    pub expr: &'static str,
    /// Postgres operator class member used for the exclusion comparison.
    /// Examples: `"="` for plain equality, `"&&"` for range overlap,
    /// `"<>"` for inequality. Emitted verbatim after `WITH`; the
    /// descriptor stores only the operator literal.
    pub with_operator: &'static str,
}

/// `EXCLUDE` constraint declaration on a model.
/// Postgres `EXCLUDE` constraints prevent multiple rows from satisfying
/// the same comparison relationship — most commonly used to prevent
/// overlapping time ranges (`tstzrange WITH &&`) or to enforce
/// uniqueness with non-equality operators. They are GiST-based by
/// default; B-tree exclusion constraints use `=` operators.
/// # PR 7 — classification
/// Adding an `EXCLUDE` constraint to a **populated** table classifies
/// as `OfflineOnly`: Postgres 18 does not accept `NOT VALID` for
/// `EXCLUDE` constraints, so the live-migration two-phase staging
/// pattern that works for `CHECK` / `NOT NULL` / FK is impossible
/// here. The empty-table case (CREATE TABLE, or an existing table
/// with zero rows) emits the constraint inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExclusionConstraintSpec {
    /// Constraint name. Emitted as `CONSTRAINT <name> EXCLUDE ...`.
    pub name: &'static str,
    /// Index method — typically `"gist"` for range overlap, or
    /// `"btree"` for `=`-based exclusion. The string is emitted
    /// verbatim into `EXCLUDE USING <method>`.
    pub using: &'static str,
    /// One element per exclusion column or expression. Order matches
    /// the source `WITH` clause and is preserved verbatim during DDL
    /// emission.
    pub elements: &'static [ExclusionElement],
    /// Optional `WHERE` predicate restricting which rows the exclusion
    /// applies to. Raw SQL — emitted verbatim. `None` means the
    /// constraint applies to every row in the table.
    pub where_clause: Option<&'static str>,
    /// `true` emits `DEFERRABLE` after the constraint body. Defaults to
    /// `false` (the Postgres default). Pairs with
    /// [`Self::initially_deferred`] for the full
    /// `DEFERRABLE INITIALLY DEFERRED` form.
    pub deferrable: bool,
    /// `true` emits `INITIALLY DEFERRED`. Only meaningful when
    /// [`Self::deferrable`] is also `true`; the macro enforces that
    /// pairing at parse time.
    pub initially_deferred: bool,
    /// Postgres extension name (e.g. `"btree_gist"`) that must be
    /// installed before this constraint can be created. `None` for
    /// stock GiST exclusions that only use range / geometric operators.
    /// # Auto-derivation
    /// The `#[model(exclusion(...))]` macro derives `Some("btree_gist")`
    /// for any `using = "gist"` exclusion whose element list contains at
    /// least one btree comparison operator (`=`, `<>`, `<`, `<=`, `>`,
    /// `>=`). The canonical scheduling shape
    /// `EXCLUDE USING gist (room_id WITH =, period WITH &&)` matches
    /// the `room_id WITH =` element needs the btree_gist operator class
    /// for `=` on `BIGINT` inside a GiST index. Pure-range exclusions
    /// (`elements = ["period WITH &&"]`) do not match and keep `None`
    /// because stock GiST handles range overlap natively.
    /// The migration system collects these dependencies and emits a
    /// `CREATE EXTENSION IF NOT EXISTS "btree_gist"` statement in the
    /// per-database bootstrap migration — adopters never write
    /// the `CREATE EXTENSION` SQL by hand, mirroring how PostGIS is
    /// auto-installed for `Geography` columns.
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

/// Compute the deterministic index name for a migration emitter.
/// Shape: `<table>_<stem-body>_<suffix>` where:
/// - `<stem-body>` is either the underscore-joined column names (for
///   [`IndexNameTarget::Columns`]) or the literal `expr` (for
///   [`IndexNameTarget::Expression`]).
/// - `<suffix>` is `idx` / `key` / `uidx` per [`IndexNameKind`].
///   Truncation rule (plan §D5): when the naïve name would exceed the
///   Postgres 63-byte identifier limit, the stem is truncated to 55 bytes
///   and an 8-character hex digest of the full pre-truncation name is
///   appended so near-duplicate inputs cannot collide.
///   The hash uses `std::hash::DefaultHasher` (SipHash-1-3) — determinism
///   within a single process is sufficient because the name is computed
///   once, emitted into a `static` literal, and never re-hashed at
///   runtime.
/// # Examples
/// ```ignore
/// use djogi::descriptor::{IndexNameKind, IndexNameTarget, index_name};
///
/// // Short, plain columns → verbatim `<table>_<cols>_idx`.
/// let name = index_name("users", IndexNameKind::NonUnique,
/// IndexNameTarget::Columns(&["email"]));
/// assert_eq!(name, "users_email_idx");
///
/// // Unique constraint → `_key` stem.
/// assert_eq!(
/// index_name("orgs", IndexNameKind::UniqueConstraint,
/// IndexNameTarget::Columns(&["org_id", "external_id"])),
/// "orgs_org_id_external_id_key"
/// );
///
/// // Expression index — table name + `expr` stem (hash suffix appears
/// // here only if the `<table>_expr_idx` string exceeds 63 bytes).
/// assert_eq!(
/// index_name("users", IndexNameKind::NonUnique,
/// IndexNameTarget::Expression),
/// "users_expr_idx"
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
    /// Lifts each `&str` in `columns` into an [`IndexColumnSpec::simple`]
    /// entry, maps `unique = true` → [`IndexKind::UniqueConstraint`] and
    /// `unique = false` → [`IndexKind::NonUnique`], and defaults every other
    /// optional field (`predicate`, `include`, `nulls_not_distinct`,
    /// `requires_out_of_transaction`, `extension_dependency`) to benign values.
    /// Not `const`: the per-column spec slice is allocated on the heap via
    /// `Box::leak` so the lifted slice satisfies the `&'static` bound that
    /// `IndexTarget::Columns` requires. The leak is intentional — `IndexSpec`
    /// values are descriptor-lifetime data that lives for the entire process,
    /// so a once-per-index leak behaves like a `static` initialiser. For
    /// truly `static` contexts (macro-emitted descriptors), construct an
    /// `IndexSpec { ... }` literal directly and put the `IndexColumnSpec`
    /// slice behind a `static` binding.
    /// # Panics
    /// Panics when `unique == true && index_type != IndexType::BTree`.
    /// PostgreSQL unique indexes are btree-only (`CREATE UNIQUE INDEX …
    /// USING gin|gist|brin|spgist|hash` is rejected by the server), so the
    /// emitter has no valid lowering for a non-btree unique index. The
    /// macro layer rejects this combination at compile time (
    /// #83); the panic here guards the same invariant for code that
    /// constructs `IndexSpec` directly via this builder.
    pub fn simple(
        name: &'static str,
        columns: &'static [&'static str],
        unique: bool,
        index_type: IndexType,
    ) -> Self {
        assert!(
            !unique || matches!(index_type, IndexType::BTree),
            "IndexSpec::simple: PostgreSQL unique indexes are btree-only; \
             cannot construct a unique index with index_type {index_type:?}. \
             Drop `unique` for a non-unique non-btree lookup index, or use \
             IndexType::BTree."
        );
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
        ComputedFieldDescriptor, ExclusionConstraintSpec, ExclusionElement, FieldDescriptor,
        FieldSqlType, GeographySubtype, IndexSpec, IndexType, ModelDescriptor, PkType,
        RangeSubtypeKind, field_descriptor, migration_shape::MigrationShape, model_descriptor,
    };

    // ── GeographySubtype Display ─────────────────────────────────────────

    /// Regression guard: `Geography { subtype: Point, srid: 4326 }`
    /// must emit exactly `"geography(Point, 4326)"` — unchanged from
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

    // ── — network family Display projection ─────────────────────
    // The migration composer projects column types by calling
    // `FieldSqlType::to_string()` (see `migrate/projection.rs` and
    // `migrate/sql.rs`). These tests pin the Display surface so a future
    // refactor that drops or renames a variant fails at descriptor-level
    // before reaching the live-DB integration gate.

    #[test]
    fn bytea_field_sql_type_displays_as_upper_bytea() {
        assert_eq!(format!("{}", FieldSqlType::Bytea), "BYTEA");
    }

    #[test]
    fn inet_field_sql_type_displays_as_upper_inet() {
        assert_eq!(format!("{}", FieldSqlType::Inet), "INET");
    }

    #[test]
    fn cidr_field_sql_type_displays_as_upper_cidr() {
        assert_eq!(format!("{}", FieldSqlType::Cidr), "CIDR");
    }

    #[test]
    fn macaddr_field_sql_type_displays_as_upper_macaddr() {
        assert_eq!(format!("{}", FieldSqlType::Macaddr), "MACADDR");
    }

    #[test]
    fn tsrange_field_sql_type_displays_as_lower_tsrange() {
        let ty = FieldSqlType::Range {
            subtype: RangeSubtypeKind::Ts,
        };
        assert_eq!(format!("{ty}"), "tsrange");
    }

    // ── IpAddr / InetAddr no-drift ────────────────────────────────
    // Both `std::net::IpAddr` and `djogi::InetAddr` lower to the same
    // `FieldSqlType::Inet` variant via the macro classifier
    // (`rust_type_to_sql` in `djogi-macros/src/model/attrs.rs`). This
    // test pins the descriptor-level equivalence so a future refactor
    // that splits them into separate variants (e.g. `InetBare` vs
    // `InetTyped`) is caught by the differ before reaching migration.

    #[test]
    fn inet_descriptor_no_drift_between_ipaddr_and_inetaddr() {
        // Both Rust types produce identical descriptor entries at the
        // SQL-type level — swapping one for the other must not create
        // a ColumnChange::ChangeType in the migration differ.
        let from_ipaddr = field_descriptor("host", FieldSqlType::Inet, false);
        let from_inetaddr = field_descriptor("host", FieldSqlType::Inet, false);

        // SQL type equivalence — the load-bearing check for the differ.
        assert_eq!(
            from_ipaddr.sql_type, from_inetaddr.sql_type,
            "IpAddr and InetAddr must map to the same FieldSqlType variant"
        );

        // Name and nullable also match (the differ compares these slots).
        assert_eq!(from_ipaddr.name, from_inetaddr.name);
        assert_eq!(from_ipaddr.nullable, from_inetaddr.nullable);
    }

    // ── Piece A — `FieldSqlType::Domain` ─────────────────────────
    // Pin the Display contract, the Clone / PartialEq round-trip on the
    // recursive variant, and a regression that the existing const
    // `field_descriptor` constructor still compiles for non-domain
    // fields. The Display surface is the load-bearing path: the
    // migration composer / differ / docs / snapshot all consume
    // `to_string()` output and never destructure the variant — keeping
    // the rendered form pinned closes the silent-rename failure mode
    // before reaching the live-DB integration gate.
    // The variant carries `name: &'static str` + `base: &'static FieldSqlType`.
    // `&'static FieldSqlType` (not `Box<FieldSqlType>`) keeps the enclosing
    // enum trivially droppable so `const fn field_descriptor(...)` continues
    // to compile in const contexts — see the variant doc comment for the
    // full rationale and the macro emits the `static BASE` binding inside
    // its `inventory::submit!` block.

    #[test]
    fn domain_sql_type_displays_as_domain_name() {
        // The rendered string is what the differ / composer / snapshot
        // see. Pin the bare domain name — not "DOMAIN positive_amount",
        // not the inner base type — so an accidental change to the
        // emitted column-type slot is caught at descriptor-level.
        // `Domain.base` is `&'static FieldSqlType` (not `Box`) so the
        // enum stays trivially droppable and `const fn field_descriptor`
        // call sites keep working. A `static` binding for the base type
        // gives the `'static` reference at zero cost.
        static BASE: FieldSqlType = FieldSqlType::Numeric;
        let ft = FieldSqlType::Domain {
            name: "positive_amount",
            base: &BASE,
        };
        assert_eq!(format!("{ft}"), "positive_amount");
    }

    #[test]
    fn domain_variant_clone_and_eq_round_trip() {
        // The `Domain` variant must Clone (shallow on `&'static`) and
        // PartialEq (recursive by-value on the dereferenced base)
        // consistently — the differ's `ColumnSchema::sql_type_text`
        // comparison happens at the rendered-string level, but
        // in-memory `ModelDescriptor` clones and the
        // `IndexSpec`-style descriptor-equality fixtures rely on the
        // structural impls.
        static BASE_NUMERIC: FieldSqlType = FieldSqlType::Numeric;
        static BASE_TEXT: FieldSqlType = FieldSqlType::Text;

        let a = FieldSqlType::Domain {
            name: "positive_amount",
            base: &BASE_NUMERIC,
        };
        let b = a.clone();
        assert_eq!(a, b);

        // Domains with the same name but different base types compare
        // unequal — mathematically correct (different inner structures
        // ARE different) and architecturally fine because the differ
        // compares only the rendered string, so two same-name Domains
        // with different bases never produce a phantom migration.
        let with_text_base = FieldSqlType::Domain {
            name: "positive_amount",
            base: &BASE_TEXT,
        };
        assert_ne!(a, with_text_base);
        assert_eq!(format!("{a}"), format!("{with_text_base}"));
    }

    #[test]
    fn field_descriptor_const_fn_unchanged_for_non_domain() {
        // Regression guard: adding the recursive `Domain` variant must
        // not break the existing `const fn field_descriptor(...)`
        // constructor for non-domain fields. The const constructor
        // is the workhorse of every fixture in `tests::` and downstream
        // crates; a stealth change to its const-ness — e.g. choosing
        // `Box<FieldSqlType>` for the recursive payload (which would
        // give the enum a non-trivial destructor that const eval
        // cannot run) — would surface here.
        // The boolean / string-literal field reads run inside a
        // `const { ... }` block so the regression check is enforced at
        // compile time. A future change that turns `field_descriptor`
        // non-const, or changes its const default behaviour, fails to
        // const-evaluate here rather than producing a tautological
        // runtime assertion (clippy::assertions_on_constants).
        const PLAIN: FieldDescriptor = field_descriptor("amt", FieldSqlType::Numeric, false);
        const _: () = {
            // String / sql_type equality and the three default-false
            // booleans together pin the full const constructor contract.
            assert!(matches!(PLAIN.sql_type, FieldSqlType::Numeric));
            assert!(!PLAIN.nullable);
            assert!(!PLAIN.unique);
            assert!(!PLAIN.indexed);
        };
        // Runtime check on the `name: &'static str` slot — string
        // equality is not const-evaluable on stable, so the assert
        // stays at runtime. `PLAIN.name` is const-evaluated; the
        // comparison runs once at test time.
        assert_eq!(PLAIN.name, "amt");
    }

    #[test]
    fn field_descriptor_const_fn_compiles_with_domain_sql_type() {
        // Companion to the regression test above — the const
        // constructor also accepts a `Domain` sql_type when called in a
        // `const` context. The static-binding pattern below is what
        // the macro emits inside `inventory::submit!` blocks.
        static BASE: FieldSqlType = FieldSqlType::Numeric;
        const DOM: FieldDescriptor = field_descriptor(
            "amount",
            FieldSqlType::Domain {
                name: "positive_amount",
                base: &BASE,
            },
            false,
        );
        assert_eq!(DOM.name, "amount");
        assert_eq!(format!("{}", DOM.sql_type), "positive_amount");
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
        // Benign defaults round-trip.
        assert_eq!(spec.predicate, None);
        assert!(spec.include.is_empty());
        assert!(!spec.nulls_not_distinct);
    }

    // ── New descriptor-shape assertions ───────────────

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

    /// #83 — `IndexSpec::simple(name, cols, unique = true,
    /// IndexType::<non-btree>)` panics. The macro layer rejects
    /// `unique(..., using = "<non-btree>")` at compile time, but the
    /// runtime builder also guards the invariant because direct calls
    /// (handwritten descriptors, future extension hooks) bypass the
    /// macro. The panic surfaces here before the descriptor is ever
    /// projected into a snapshot or emitted as DDL.
    #[test]
    #[should_panic(expected = "PostgreSQL unique indexes are btree-only")]
    fn index_spec_simple_panics_on_unique_with_gin() {
        use super::{IndexSpec, IndexType};
        let _ = IndexSpec::simple("u", &["payload"], true, IndexType::Gin);
    }

    #[test]
    #[should_panic(expected = "PostgreSQL unique indexes are btree-only")]
    fn index_spec_simple_panics_on_unique_with_gist() {
        use super::{IndexSpec, IndexType};
        let _ = IndexSpec::simple("u", &["loc"], true, IndexType::Gist);
    }

    #[test]
    #[should_panic(expected = "PostgreSQL unique indexes are btree-only")]
    fn index_spec_simple_panics_on_unique_with_hash() {
        use super::{IndexSpec, IndexType};
        let _ = IndexSpec::simple("u", &["slug"], true, IndexType::Hash);
    }

    #[test]
    #[should_panic(expected = "PostgreSQL unique indexes are btree-only")]
    fn index_spec_simple_panics_on_unique_with_brin() {
        use super::{IndexSpec, IndexType};
        let _ = IndexSpec::simple("u", &["happened_at"], true, IndexType::Brin);
    }

    #[test]
    #[should_panic(expected = "PostgreSQL unique indexes are btree-only")]
    fn index_spec_simple_panics_on_unique_with_spgist() {
        use super::{IndexSpec, IndexType};
        let _ = IndexSpec::simple("u", &["path"], true, IndexType::Spgist);
    }

    /// Negative cases — non-unique non-btree indexes are valid and must
    /// not panic. The runtime guard fires only on `unique = true` +
    /// non-btree; covering `unique = false` cases here protects the
    /// non-unique surface from accidental over-tightening.
    #[test]
    fn index_spec_simple_accepts_non_unique_with_any_method() {
        use super::{IndexSpec, IndexType};
        for ty in [
            IndexType::BTree,
            IndexType::Gin,
            IndexType::Gist,
            IndexType::Hash,
            IndexType::Brin,
            IndexType::Spgist,
        ] {
            let spec = IndexSpec::simple("n", &["col"], false, ty);
            assert!(matches!(spec.kind, super::IndexKind::NonUnique));
            assert_eq!(spec.index_type, ty);
        }
    }

    #[test]
    fn index_spec_equality_and_clone_preserve_new_fields() {
        use super::{
            IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget,
            IndexType,
        };
        // Construct an `IndexSpec` literal that exercises *every* new
        // Field at a non-default value. If `Clone` or
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

    // ── index_name deterministic helper ──────────────

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
        // Constructing a minimal descriptor twice — once per new variant
        // and asserting `pk_column` answers `Some("id")` is the cleanest
        // way to pin the PkType addition.
        use super::super::relation::{OnDelete, RelationKind};

        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            unique: true,
            indexed: true,
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];

        let _ = (OnDelete::Restrict, RelationKind::ForeignKey);

        for pk in [PkType::HeerIdDesc, PkType::RanjIdDesc] {
            let desc = ModelDescriptor {
                ..model_descriptor("Desc", "descs", pk, FIELDS)
            };
            assert_eq!(desc.pk_column(), Some("id"));
        }
    }

    /// Construct a minimal `ModelDescriptor` by hand and verify that
    /// `MigrationShape::from_descriptor` produces sensible column shapes for
    /// the framework-injected fields plus one user field.
    /// Hand-constructing a `ModelDescriptor` is feasible here because all
    /// fields are `&'static …` slices or options that can be satisfied with
    /// `'static` literals. This unit test keeps the helper covered even
    /// without relying on `#[model]`-emitted data.
    #[test]
    fn migration_shape_from_minimal_descriptor() {
        use super::super::relation::{OnDelete, RelationKind};

        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                unique: true,
                indexed: true,
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                ..field_descriptor("label", FieldSqlType::Text, false)
            },
        ];

        // Suppress unused-import warnings from the wildcard bring-in above
        // `OnDelete` and `RelationKind` are imported because the `use` block
        // is required to satisfy the FieldDescriptor struct literal, even
        // though neither variant is used in the literal values.
        let _ = (OnDelete::Restrict, RelationKind::ForeignKey);

        let desc = ModelDescriptor {
            ..model_descriptor("Minimal", "minimals", PkType::HeerId, FIELDS)
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

    // ── has_gist_on_geography helper ────────────────────────────────────

    // Static descriptors used by has_gist_on_geography tests.
    // `ModelDescriptor` requires `'static` slices for `fields` and `indexes`,
    // so the entire data set must be declared as module-level statics rather
    // than local temporaries.

    static T11_GEO_FIELD: FieldDescriptor = FieldDescriptor {
        ..field_descriptor(
            "boundary",
            FieldSqlType::Geography {
                subtype: GeographySubtype::Polygon,
                srid: 4326,
            },
            false,
        )
    };

    static T11_TEXT_FIELD: FieldDescriptor = FieldDescriptor {
        ..field_descriptor("label", FieldSqlType::Text, false)
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
            indexes: std::slice::from_ref(&T11_GIST_INDEX),
            ..model_descriptor(
                "Region",
                "regions",
                PkType::HeerId,
                std::slice::from_ref(&T11_GEO_FIELD),
            )
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
            ..model_descriptor(
                "Region",
                "regions",
                PkType::HeerId,
                std::slice::from_ref(&T11_GEO_FIELD),
            )
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
            indexes: std::slice::from_ref(&T11_BTREE_INDEX),
            ..model_descriptor(
                "Region",
                "regions",
                PkType::HeerId,
                std::slice::from_ref(&T11_GEO_FIELD),
            )
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
            indexes: std::slice::from_ref(&T11_GIST_ON_TEXT),
            ..model_descriptor(
                "Region",
                "regions",
                PkType::HeerId,
                std::slice::from_ref(&T11_TEXT_FIELD),
            )
        };
        assert!(
            !desc.has_gist_on_geography(),
            "expected false: GiST on a text column is not spatial acceleration"
        );
    }

    // ── — MigrationShape collects extension deps from exclusions ──

    static EXCL_BTREE_GIST_ELEMENTS: &[ExclusionElement] = &[
        ExclusionElement {
            expr: "room_id",
            with_operator: "=",
        },
        ExclusionElement {
            expr: "period",
            with_operator: "&&",
        },
    ];

    static EXCL_BTREE_GIST: ExclusionConstraintSpec = ExclusionConstraintSpec {
        name: "bookings_no_overlap",
        using: "gist",
        elements: EXCL_BTREE_GIST_ELEMENTS,
        where_clause: None,
        deferrable: false,
        initially_deferred: false,
        extension_dependency: Some("btree_gist"),
    };

    /// `MigrationShape::from_descriptor` must include the
    /// `ExclusionConstraintSpec::extension_dependency` value in
    /// `required_extensions`. This is the contract proof that the macro
    /// auto-derived `Some("btree_gist")` reaches the migration-emission
    /// layer.
    #[test]
    fn migration_shape_collects_btree_gist_from_exclusion_constraint() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let desc = ModelDescriptor {
            exclusion_constraints: std::slice::from_ref(&EXCL_BTREE_GIST),
            ..model_descriptor("Booking", "bookings", PkType::HeerId, FIELDS)
        };
        let shape = MigrationShape::from_descriptor(&desc);
        assert!(
            shape.required_extensions.contains("btree_gist"),
            "btree_gist must surface in required_extensions from exclusion spec; got {:?}",
            shape.required_extensions,
        );
        assert_eq!(
            shape.exclusion_constraints.len(),
            1,
            "shape.exclusion_constraints must carry the exclusion verbatim",
        );
    }

    /// `None` extension dependency leaves
    /// `required_extensions` untouched. Mirrors the index path.
    #[test]
    fn migration_shape_skips_none_exclusion_extension() {
        static ELEMENTS: &[ExclusionElement] = &[ExclusionElement {
            expr: "period",
            with_operator: "&&",
        }];
        static EXCL: ExclusionConstraintSpec = ExclusionConstraintSpec {
            name: "period_overlap",
            using: "gist",
            elements: ELEMENTS,
            where_clause: None,
            deferrable: false,
            initially_deferred: false,
            extension_dependency: None,
        };

        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let desc = ModelDescriptor {
            exclusion_constraints: std::slice::from_ref(&EXCL),
            ..model_descriptor("Reservation", "reservations", PkType::HeerId, FIELDS)
        };
        let shape = MigrationShape::from_descriptor(&desc);
        assert!(
            shape.required_extensions.is_empty(),
            "extension_dependency=None must not register any extension; got {:?}",
            shape.required_extensions,
        );
    }

    // ── 1 — proxy descriptor field defaults ─────────────────────

    /// `proxy_for` defaults to `None` for every non-proxy descriptor.
    /// Locks the struct-layout-stability convention: adding a new field on
    /// `ModelDescriptor` must default to a no-op shape so existing literal
    /// sites that go through `model_descriptor(...)` keep compiling.
    #[test]
    fn proxy_for_field_defaults_to_none() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let desc = ModelDescriptor {
            ..model_descriptor("V", "vs", PkType::HeerId, FIELDS)
        };
        assert!(desc.proxy_for.is_none());
    }

    /// `default_filter_sql` defaults to `None` for every non-proxy descriptor.
    /// Mirror of `proxy_for_field_defaults_to_none` — the two fields ship
    /// together and share the no-op default convention.
    #[test]
    fn default_filter_sql_field_defaults_to_none() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let desc = ModelDescriptor {
            ..model_descriptor("V", "vs", PkType::HeerId, FIELDS)
        };
        assert!(desc.default_filter_sql.is_none());
    }

    /// Both proxy fields can be populated together — sanity check that the
    /// struct layout accepts the proxy shape the macro emits.
    /// Hand-constructs the descriptor with non-default values for both new
    /// fields and asserts they round-trip. Locks the field surface so
    /// renaming or retyping either field surfaces here.
    #[test]
    fn proxy_fields_round_trip_populated_values() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let desc = ModelDescriptor {
            proxy_for: Some("Vehicle"),
            default_filter_sql: Some("active = TRUE"),
            ..model_descriptor("ActiveVehicle", "vehicles", PkType::HeerId, FIELDS)
        };
        assert_eq!(desc.proxy_for, Some("Vehicle"));
        assert_eq!(desc.default_filter_sql, Some("active = TRUE"));
    }

    // ── 1 — computed-field descriptor defaults ───────────────────

    /// `computed_fields` defaults to the empty slice for every model
    /// without `#[computed(sql = "...")]` attributes — the common case.
    /// Mirrors the proxy_for / default_filter_sql default-shape tests
    /// Locks the struct-layout-stability convention so adding
    /// `computed_fields` does not break existing descriptor literal
    /// sites that go through the `model_descriptor(...)` factory spread.
    #[test]
    fn computed_fields_defaults_to_empty_slice() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        let desc = ModelDescriptor {
            ..model_descriptor("V", "vs", PkType::HeerId, FIELDS)
        };
        assert!(desc.computed_fields.is_empty());
    }

    /// A descriptor populated with computed-field entries round-trips
    /// every field correctly. Locks the `ComputedFieldDescriptor`
    /// shape (name / sql / value_type) so any rename or retyping
    /// surfaces here.
    #[test]
    fn computed_field_descriptor_struct_shape() {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("base_price", FieldSqlType::DoublePrecision, false)
        }];
        static COMPUTED: &[ComputedFieldDescriptor] = &[ComputedFieldDescriptor {
            name: "total_price",
            sql: "base_price * (1.0 + tax_rate)",
            value_type: FieldSqlType::DoublePrecision,
        }];
        let desc = ModelDescriptor {
            computed_fields: COMPUTED,
            ..model_descriptor("Vehicle", "vehicles", PkType::HeerId, FIELDS)
        };
        assert_eq!(desc.computed_fields.len(), 1);
        let c = &desc.computed_fields[0];
        assert_eq!(c.name, "total_price");
        assert_eq!(c.sql, "base_price * (1.0 + tax_rate)");
        assert_eq!(c.value_type, FieldSqlType::DoublePrecision);
    }

    // ── — FieldSqlType::Varchar ───────────────────────────────────

    #[test]
    fn varchar_display_formats_with_bound() {
        // `Varchar(n)` must produce exactly `"VARCHAR(n)"` — the
        // migration differ uses `ColumnSchema::sql_type` (a `String`)
        // for equality checks, so the serialised form is the contract.
        assert_eq!(FieldSqlType::Varchar(100).to_string(), "VARCHAR(100)");
        assert_eq!(FieldSqlType::Varchar(1).to_string(), "VARCHAR(1)");
        assert_eq!(
            FieldSqlType::Varchar(10_485_760).to_string(),
            "VARCHAR(10485760)"
        );
    }

    #[test]
    fn text_display_unchanged_after_varchar_addition() {
        // `Text` must still serialise as `"TEXT"` — no regression from
        // the new `Varchar` variant landing before it in the `match`.
        assert_eq!(FieldSqlType::Text.to_string(), "TEXT");
    }
}

#[cfg(test)]
mod protected_field_metadata_tests {
    use super::{
        FieldDescriptor, FieldSqlType, ProtectedFieldMetadata, RedactionPolicy, RetentionLabel,
        Sensitivity, field_descriptor,
    };

    /// Constructing a `ProtectedFieldMetadata` with non-default variants of
    /// every enum field locks the struct shape to §1 D5 — adding or
    /// renaming a field forces this test to fail before any consumer is
    /// touched.
    #[test]
    fn protected_field_metadata_round_trips_non_default_variants() {
        let pfm = ProtectedFieldMetadata {
            sensitivity: Sensitivity::Pii,
            rationale: "GDPR Art. 6(1)(b) — notification delivery",
            redaction: RedactionPolicy::Mask,
            codec: Some("aes256_gcm_v1"),
            retention: RetentionLabel::Extended,
        };
        assert_eq!(pfm.sensitivity, Sensitivity::Pii);
        assert_eq!(pfm.rationale, "GDPR Art. 6(1)(b) — notification delivery");
        assert_eq!(pfm.redaction, RedactionPolicy::Mask);
        assert_eq!(pfm.codec, Some("aes256_gcm_v1"));
        assert_eq!(pfm.retention, RetentionLabel::Extended);
    }

    /// `Default` produces the neutral "ordinary field" state. The macro
    /// relies on these defaults when eliding unset attribute knobs.
    #[test]
    fn default_protected_field_metadata_is_neutral() {
        let pfm = ProtectedFieldMetadata::default();
        assert_eq!(pfm.sensitivity, Sensitivity::None);
        assert_eq!(pfm.rationale, "");
        assert_eq!(pfm.redaction, RedactionPolicy::None);
        assert_eq!(pfm.codec, None);
        assert_eq!(pfm.retention, RetentionLabel::Standard);
    }

    /// Enum-level `Default` impls match the struct-level neutral state
    /// independently checked so future refactors that move the defaults
    /// onto the enums (or back) cannot drift.
    #[test]
    fn enum_defaults_match_neutral_metadata() {
        assert_eq!(Sensitivity::default(), Sensitivity::None);
        assert_eq!(RedactionPolicy::default(), RedactionPolicy::None);
        assert_eq!(RetentionLabel::default(), RetentionLabel::Standard);
    }

    /// Logging consumers compare `Sensitivity` ordinals to gate field
    /// exposure — pin the variant ordering so a future variant insertion
    /// in the middle of the enum surfaces as a test failure.
    #[test]
    fn sensitivity_ordering_pins_least_to_most_sensitive() {
        assert!(Sensitivity::None < Sensitivity::Internal);
        assert!(Sensitivity::Internal < Sensitivity::Pii);
        assert!(Sensitivity::Pii < Sensitivity::Sensitive);
        assert!(Sensitivity::Sensitive < Sensitivity::Secret);
    }

    /// `FieldDescriptor::type_change_using` defaults to `None`
    /// in the [`field_descriptor`] constructor and accepts a `'static`
    /// adopter expression when set. A literal-level smoke test prevents
    /// accidental shape regressions; this slot has no projection arm that
    /// would surface a missing default at runtime, so an unset default
    /// would otherwise only break the macro's generated code.
    #[test]
    fn field_descriptor_accepts_type_change_using_none_and_some() {
        let no_using = FieldDescriptor {
            ..field_descriptor("ordinary", FieldSqlType::Text, false)
        };
        assert!(
            no_using.type_change_using.is_none(),
            "constructor must default `type_change_using` to None for every \
             new descriptor — adding the attribute is opt-in",
        );

        let with_using = FieldDescriptor {
            type_change_using: Some("kind::uuid"),
            ..field_descriptor("kind", FieldSqlType::Uuid, false)
        };
        assert_eq!(with_using.type_change_using, Some("kind::uuid"));
    }

    /// `FieldDescriptor` accepts both `protected: None` (the default that
    /// the macro emits today) and `protected: Some(_)` (emitted when
    /// `#[field(protected(...))]` is wired). A literal-level
    /// smoke test prevents accidental shape regressions.
    #[test]
    fn field_descriptor_accepts_protected_none_and_some() {
        let none_desc = FieldDescriptor {
            ..field_descriptor("ordinary", FieldSqlType::Text, false)
        };
        assert!(none_desc.protected.is_none());

        let some_desc = FieldDescriptor {
            unique: true,
            indexed: true,
            protected: Some(ProtectedFieldMetadata {
                sensitivity: Sensitivity::Pii,
                rationale: "Notification delivery",
                redaction: RedactionPolicy::Mask,
                codec: Some("aes256_gcm_v1"),
                retention: RetentionLabel::Standard,
            }),
            ..field_descriptor("email", FieldSqlType::Text, false)
        };
        let pfm = some_desc.protected.expect("constructed with Some(...)");
        assert_eq!(pfm.sensitivity, Sensitivity::Pii);
        assert_eq!(pfm.codec, Some("aes256_gcm_v1"));
    }
}

/// The Rust source type of a model field when its Rust type does not have a
/// native `tokio_postgres::ToSql` / `FromSql` implementation that targets the
/// desired SQL column type.
/// When a `#[model]` field uses one of these Rust types, the `#[model]` macro
/// emits bind shims that widen the value to a wire-compatible type before
/// binding, and decode shims that narrow the wire value back with a
/// bounds-checked `try_from` / `to_u64`. The migration projection layer
/// (`djogi::migrate::projection::field_type_check`) reads this discriminator
/// to emit a per-column range CHECK that rejects external writes outside the
/// Rust type's representable range.
/// `None` on `FieldDescriptor` means the field maps directly to the SQL
/// carrier (e.g. `i32 → INTEGER`, `i64 → BIGINT`, `String → TEXT`); no
/// shim or range CHECK is emitted.
/// **Two roles, one discriminator.** Not every `Some(...)` value implies
/// shim emission. The integer-widening variants
/// (`I8` / `U8` / `U16` / `U32` / `U64`) drive both bind/decode shims AND
/// the type-derived CHECK projection (the Rust type has no native `ToSql`
/// targeting the SQL carrier, or its native impl targets the wrong Postgres
/// type). The structural-bounds variants (`Decimal`) drive **only** the
/// type-derived CHECK projection — `rust_decimal::Decimal` already has the
/// correct `ToSql for NUMERIC` / `FromSql from NUMERIC` impls in
/// `postgres-types`, so no bind / decode shim is required; the discriminator
/// exists solely to disambiguate adopter `Decimal → NUMERIC` columns from
/// `u64 → NUMERIC` columns at projection time. Both kinds of
/// variant share the same descriptor slot because both feed the same
/// projection dispatch in `migrate/projection.rs::field_type_check`.
/// # Wire-type mapping
/// | `RustSourceType` | SQL carrier | Bind shim | Decode shim | CHECK projection |
/// |------------------|--------------------|--------------------|----------------------|---------------------------------------|
/// | `I8` | SMALLINT (INT2) | `i16::from(v)` | `i8::try_from(i16)` | range `−128..=127` |
/// | `U8` | SMALLINT (INT2) | `i16::from(v)` | `u8::try_from(i16)` | range `0..=255` |
/// | `U16` | INTEGER (INT4) | `i32::from(v)` | `u16::try_from(i32)` | range `0..=65535` |
/// | `U32` | BIGINT (INT8) | `i64::from(v)` | `u32::try_from(i64)` | range `0..=4294967295` |
/// | `U64`            | NUMERIC            | `Decimal::from(v)` | `Decimal::to_u64()`  | range + integrality (`col = trunc`)   |
/// | `Decimal` | NUMERIC | none (native impl) | none (native impl) | structural `mantissa ≤ 2^96 − 1` |
/// # Why not `i8` in a native `ToSql` impl?
/// `postgres-types` does implement `ToSql` for `i8`, but it maps to the
/// Postgres pseudo-type `"char"` (a 1-byte type used internally by system
/// catalogs), **not** to `SMALLINT`. A djogi model field typed `i8` carries
/// a `SMALLINT` column (range-checked to `−128..=127`) so adopters get a
/// proper SQL integer type. The bind shim bridges the gap.
/// # Why not `u32` in a native `ToSql` impl?
/// `postgres-types` implements `ToSql for u32` targeting `OID`, not `BIGINT`.
/// A djogi `u32` field carries a `BIGINT` column (CHECK `0..=4294967295`);
/// the bind shim bridges the gap.
/// # Why mark `Decimal` here when there is no shim?
/// `rust_decimal::Decimal` (96-bit mantissa, scale 0..=28) admits at most 29
/// significant decimal digits; Postgres `NUMERIC` is unbounded. Without a
/// CHECK, an external writer (raw SQL migration, BI tool, sister application)
/// can land a value with 100 digits or scale 50, and the typed `SELECT`
/// decode via `rust_decimal::Decimal::FromSql` then fails with
/// `DjogiError::Decode` — the same single-bad-row poisoning class as the
/// integer family. closes the hole by projecting a structural CHECK
/// on adopter Decimal columns. The discriminator value `Decimal` exists so the
/// projection layer can distinguish a `u64 → NUMERIC` column (range +
/// integrality CHECK) from an adopter `Decimal → NUMERIC` column (structural
/// CHECK). Bare `Numeric` with `rust_source_type: None` would conflate the
/// two and emit the wrong CHECK shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustSourceType {
    /// `i8` — stored as SMALLINT; range `−128..=127`.
    I8,
    /// `u8` — stored as SMALLINT; range `0..=255`.
    U8,
    /// `u16` — stored as INTEGER; range `0..=65535`.
    U16,
    /// `u32` — stored as BIGINT; range `0..=4294967295`.
    U32,
    /// `u64` — stored as bare NUMERIC; range `0..=18446744073709551615`.
    /// Uses bare NUMERIC (no precision/scale) so Postgres does not round
    /// fractional inputs before the CHECK constraint evaluates. The
    /// projection layer emits `col >= 0 AND col <= u64::MAX AND col = trunc(col)`
    /// to reject out-of-range and fractional values at the DB level.
    U64,
    /// `rust_decimal::Decimal` — stored as bare NUMERIC; structural bound
    /// enforces the 96-bit mantissa and scale ≤ 28 limits of the Rust type.
    /// No bind / decode shim is emitted — `rust_decimal::Decimal` already
    /// implements `postgres_types::ToSql for NUMERIC` and the matching
    /// `FromSql`. The discriminator exists solely so the projection layer
    /// can emit `scale(col) <= 28 AND abs(col) * power(10::numeric,
    /// scale(col)) <= 79228162514264337593543950335` against an adopter
    /// Decimal column instead of the U64 range CHECK that fires when
    /// `rust_source_type == Some(U64)`. .
    Decimal,
}

/// Metadata for a single model field.
/// `ModelDescriptor::fields` is the complete schema contract — it
/// INCLUDES the framework-injected columns (`id`, `created_at`,
/// `updated_at`) before any user-declared fields. Consumers
/// (migration differ, admin UI, `djogi docs`, RLS generator) iterate
/// `descriptor.fields` as the single schema source and never
/// synthesize framework columns out-of-band.
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
    /// advisory warning when `lazy = true` or `outbox = "ignore"`
    /// is set without a rationale — surfaces forgotten context at review time.
    pub rationale: Option<&'static str>,
    /// `#[field(outbox = "ignore")]` — exclude the field from the outbox
    /// payload. Declared in ; outbox writes land in .
    pub outbox_exclude: bool,
    /// `#[field(sequence_within = "parent_fk_column")]` — parent FK
    /// column that scopes this field's monotonic sequence. When
    /// `Some(col)`, the emitted `Model::create` runs a counter
    /// upsert against `<table>_seq_<col>` inside the caller's
    /// atomic scope before inserting the row, captures the returned
    /// `last_seq`, and writes it into this field. .
    /// `None` for every field that is not scope-sequenced. The
    /// macro enforces that at most one field per model carries this
    /// attribute (multi-scope sequencing is a future extension).
    pub sequence_within: Option<&'static str>,
    /// Override the index method for this field's implicit index.
    /// `None` falls back to `IndexType::BTree`. Declared in ;
    /// migration generation for non-BTree methods lands in .
    pub index_type: Option<IndexType>,

    // ── Relation metadata ────────────────────────────────
    /// Relation cardinality, when this field stores the `Source`-side column
    /// of an FK / O2O relation. `None` for every scalar column — the macro
    /// recognises only `ForeignKey<T>`, `Option<ForeignKey<T>>`,
    /// `OneToOneField<T>`, and `Option<OneToOneField<T>>` as relation shapes;
    /// anything else keeps this at `None` and the downstream consumers
    /// (DDL, prefetch planning) treat the column as a
    /// scalar.
    pub relation_kind: Option<RelationKind>,

    /// `#[field(on_delete = "...")]` value, meaningful only when
    /// `relation_kind.is_some()`. A `None` here with `relation_kind = Some`
    /// falls back to `OnDelete::Restrict` at DDL-emission time
    /// matching the framework's cascade-off-by-default stance. The
    /// descriptor stores the parsed value (not the raw string) so every
    /// downstream consumer works from the same enum.
    pub on_delete: Option<OnDelete>,

    /// Fully-qualified target type name (e.g. `"Owner"` for
    /// `ForeignKey<Owner>`). `None` for scalar columns. Used by the
    /// migration emitter to produce `REFERENCES {target_table}(id)` clauses,
    /// and by the prefetch planner when it needs to reflect on the
    /// target's `ModelDescriptor`. Stored as the Rust type name — not a
    /// table name — so it can be matched against the registered
    /// `ModelDescriptor::type_name` without re-deriving the identifier.
    pub target_type_name: Option<&'static str>,

    /// `true` when this column's `ForeignKey<T>` / `OneToOneField<T>` target
    /// is the same model the field belongs to — i.e. a *self-FK* edge.
    /// Metadata-only flag the recursive-query
    /// builder (B2) reads to validate `RelationPath<T, T>` use at compile
    /// time and to surface multi-edge ambiguity at runtime.
    /// Always `false` for non-relation columns and for FK/O2O columns whose
    /// target type's last-segment ident differs from the source model's
    /// short name. The check is name-based — same heuristic the descriptor
    /// already uses for `target_type_name` lookup against
    /// `ModelDescriptor::type_name`.
    pub is_self_fk: bool,

    /// Forward-declared visage-per-scope mapping. emits an
    /// empty slice; extends `#[field(expose(scope = "column"))]`
    /// parsing to populate this without reshaping the descriptor. The
    /// slice shape is `&[(scope_name, emitted_column_alias)]` — the
    /// visage emitter projects the column under the aliased name
    /// when the given scope is active.
    pub visage_map: &'static [(&'static str, &'static str)],

    /// `#[field(protected(...))]` metadata. `None` for fields that did
    /// not opt in. Descriptor surface only; macro parsing wires it;
    /// the live-migration classifier consumes for transition routing.
    /// Distinct from `Sensitivity::None` inside a `Some(_)` value: an
    /// outer `None` means the adopter never invoked `protected(...)`,
    /// while `Some(ProtectedFieldMetadata { sensitivity: None, .. })`
    /// is an explicit "ordinary field" assertion. Auditing tools that
    /// surface unannotated fields rely on this distinction.
    pub protected: Option<ProtectedFieldMetadata>,

    /// `#[field(default_volatility = "...")]` adopter-supplied override
    /// for the default expression's Postgres volatility classification.
    /// Stores the parsed override; the consumer (the
    /// `pg_volatility.rs` lookup table that classifies default
    /// expressions during compose).
    /// `None` means "fall through to the static `pg_volatility.rs`
    /// lookup at compose time"; `Some(variant)` means the adopter has
    /// asserted the volatility class for a default expression Djogi
    /// could not classify (typically a user-defined function or an
    /// extension call). The override is a pure assertion — the
    /// classifier trusts it without re-checking, so a wrong override
    /// can produce unsafe online-migration plans. The macro enforces:
    /// - Only on fields that also carry `default = "..."` (otherwise
    ///   there is no expression to classify).
    /// - Only the three documented variants (`"immutable"`, `"stable"`,
    ///   `"volatile"`); unknown strings are rejected at macro-expansion
    ///   time.
    pub default_volatility_override: Option<DefaultVolatility>,

    /// `#[field(generated = "<expr>", stored = true)]` stored generated
    /// column metadata. PR 7 — Postgres 18 stored generated
    /// columns. `None` for every regular column.
    /// When `Some`, the column emits as
    /// `<name> <type> GENERATED ALWAYS AS (<expression>) STORED` and
    /// the live-migration classifier routes add / change on populated
    /// tables to `OfflineOnly`. The empty-table case emits inline.
    /// Distinct from the FTS-tsvector path on `ModelDescriptor::fts`:
    /// FTS is a model-level convenience that emits a hardcoded
    /// `GENERATED ALWAYS AS (to_tsvector(...)) STORED` column;
    /// `generated` is the general-purpose adopter-controlled form.
    pub generated: Option<GeneratedColumnSpec>,

    /// Composition-derive provenance — the name of the composition
    /// trait that contributed this field to the model's schema, when
    /// applicable. 5.
    /// Two callers populate this slot today:
    /// - `#[model(auditable)]` on a model with a `created_by` field
    ///   sets `composed_via: Some("Auditable")` on that field.
    /// - `#[model(soft_deletable)]` on a model with a `deleted_at`
    ///   field sets `composed_via: Some("SoftDeletable")` on that
    ///   field.
    ///   `None` for every user-declared field that is not contributed
    ///   by a recognised composition opt-in — including framework
    ///   columns (`id`, `created_at`, `updated_at`) and any
    ///   adopter-declared `created_by` / `deleted_at` field whose model
    ///   did not opt into the matching `#[model(...)]` flag.
    ///   Migration emission is byte-identical for composed vs hand-
    ///   declared columns — this slot is provenance metadata only,
    ///   surfaced to `djogi docs` and admin-UI consumers that want to
    ///   distinguish framework-contributed columns from adopter-
    ///   authored ones. The migration differ does NOT key off this
    ///   field; a `composed_via: Some("Auditable")` column compares
    ///   identical to a hand-declared `created_by: Option<String>`
    ///   column under `diff_schemas`.
    ///   **Do not use as a behavioral gate.** Today the tag is keyed off
    ///   the model attribute (`auditable` / `soft_deletable`) AND the
    ///   canonical column name (`created_by` / `deleted_at`); any future
    ///   per-model column-rename path (e.g.
    ///   `#[model(soft_deletable(column = "trashed_at"))]` overriding
    ///   `<M as SoftDeletable>::COLUMN`) would re-introduce a different
    ///   kind of mismatch — the descriptor field would still be tagged
    ///   against the renamed column's name, but a behavioural-gate
    ///   consumer that relied on this tag would key off a stale
    ///   `"deleted_at"` literal and silently miss the renamed column.
    ///   Future consumers that need to reason about composition
    ///   behaviour must read it from the typed trait surface (`<M as
    /// SoftDeletable>` / `<M as Auditable>`) rather than from this
    ///   metadata slot.
    pub composed_via: Option<&'static str>,

    /// Rust source type discriminator for fields whose Rust type has no
    /// native `tokio_postgres::ToSql` / `FromSql` impl targeting the SQL
    /// carrier, or whose native impl maps to the **wrong** Postgres type.
    /// When `Some`, the `#[model]` macro emits bind shims (widen before
    /// binding) and decode shims (narrow with a bounds-checked `try_from`).
    /// The migration projection layer reads this field to emit a per-column
    /// range CHECK that rejects external writes outside the Rust type's
    /// representable range.
    /// `None` for framework columns (`id`, `created_at`, `updated_at`) and
    /// for user fields whose Rust type maps directly to the SQL carrier
    /// (`i16 → SMALLINT`, `i32 → INTEGER`, `i64 → BIGINT`, `String → TEXT`,
    /// etc.). See [`RustSourceType`] for the full mapping table.
    /// (— integer widening bind/decode shims).
    pub rust_source_type: Option<RustSourceType>,

    /// Adopter-supplied `#[field(check = "<sql expr>")]` raw-SQL CHECK
    /// expression. `None` for the common case (no adopter CHECK).
    /// **Raw SQL escape.** The expression is treated identically to a raw
    /// SQL fragment — djogi does NOT parse, sanitize, or validate the
    /// SQL beyond rejecting empty / whitespace-only strings at macro-parse
    /// time. The expression is emitted verbatim into both the
    /// `CREATE TABLE … CONSTRAINT <table>_<column>_check CHECK (<expr>)`
    /// form and the `ALTER TABLE … ADD CONSTRAINT … CHECK (<expr>)` form
    /// in migrations. Adopters are responsible for the expression's
    /// correctness; the same "this is your `unsafe`" cultural posture
    /// described in `docs/spec/raw-sql-escape-hatches.md` applies — at
    /// review time, every `#[field(check = "...")]` callsite should be
    /// reviewable as raw SQL.
    /// **Combination with type-derived CHECKs.** When a column also
    /// receives a type-derived CHECK (e.g. an adopter `u32` field with
    /// `#[field(check = "port > 0")]`), the projection layer combines the
    /// two with logical `AND` into a single constraint slot (`<table>_<col>_check`).
    /// The combined expression is `(<type-derived>) AND (<adopter>)`.
    /// Both clauses must pass for an INSERT / UPDATE to land. The single
    /// constraint slot keeps the ADD / DROP / AMEND lifecycle in the
    /// differ unchanged.
    /// .
    pub check_sql: Option<&'static str>,

    /// Adopter-supplied `#[field(comment = "<text>")]` free-text column
    /// comment, lowered by the migration composer to
    /// `COMMENT ON COLUMN <table>.<column> IS '<text>'`. Emitted
    /// immediately after the column appears in either `CREATE TABLE`
    /// (initial creation) or `ADD COLUMN` (later addition); the differ
    /// surfaces value changes via
    /// [`crate::migrate::diff::ColumnChange::SetComment`].
    /// `None` for fields that declare no comment — the common case.
    /// **Quoting.** The string is the adopter's prose, verbatim — the
    /// composer owns single-quote escaping at SQL-emission time (per
    /// the standard Postgres lexer rule that a doubled apostrophe
    /// `''` inside a `'…'` literal collapses to one apostrophe under
    /// `standard_conforming_strings = on`, which is the PG 18 default
    /// and is required by djogi). The descriptor carries the original
    /// text so adopters round-tripping comments through `pg_dump` /
    /// `pg_description` see exactly what they wrote.
    /// Djogi#217).
    pub comment: Option<&'static str>,

    /// `#[field(strict_id_check)]` or model-wide `#[model(strict_ids)]`
    /// opt-in structural CHECK constraint on HeerId / RanjId columns.
    /// **Default off.** When `false` (the common case), no extra CHECK is
    /// projected for HeerId / RanjId columns; the BIGINT / UUID column
    /// type itself accepts any value (including, e.g., a UUIDv4 written
    /// into a UUID column intended to hold UUIDv8 / RanjId payloads).
    /// External writers can land structurally malformed IDs that survive
    /// the INSERT and only surface as decode failures when later read
    /// back through the typed Rust path (`HeerId::from_i64` rejects
    /// negative values; `RanjId::from_uuid` rejects non-v8 / non-RFC4122
    /// variants).
    /// **When `true`,** the projection layer emits a structural CHECK
    /// on the column at migration time based on the column's HeerRanjID
    /// **semantic family** — resolved from the descriptor's [`PkType`]
    /// (for the framework `id` column or for the FK target via
    /// `type_to_pk_family`), not from the resolved SQL type string:
    /// * `HeerId` / `HeerIdDesc` family (BIGINT carrier) → `<col> >= 0`.
    ///   The single invariant `HeerId::from_i64` enforces is `bit 63 = 0`
    ///   (i.e. the value is non-negative when interpreted as `i64`).
    ///   All other 63 bits (41 timestamp + 9 node + 13 sequence) are
    ///   structurally valid.
    /// * `RanjId` / `RanjIdDesc` family (UUID carrier) → version=8 and
    ///   variant=RFC4122. `RanjId::from_uuid` rejects every UUID whose
    ///   version nibble is not 8 or whose variant high bits are not
    ///   `10`. The flip-mask for `RanjIdDesc` preserves the version +
    ///   variant nibbles, so both ascending and descending RanjId
    ///   variants share this CHECK.
    /// * Any other semantic family — `Serial`, `Custom`, `Composite`,
    ///   `None` — no CHECK is emitted. This is the FK-to-Serial,
    ///   FK-to-Custom, and FK-to-Composite case (e.g. an `FK<Vehicle>`
    ///   where `Vehicle` has `pk = Serial`, or `pk = AppSnowflakeId`
    ///   custom): the macro propagates the opt-in flag to every FK
    ///   column when `#[model(strict_ids)]` is set, and the projection
    ///   silently skips columns whose target PK family is not HeerId /
    ///   RanjId. Custom PKs with a `BIGINT` or `UUID` inner SQL_TYPE
    ///   are NOT coerced into the HeerRanjID family — the family
    ///   dispatch correctly maps them to `None` despite the SQL-carrier
    ///   collision.
    ///   **Performance.** The HeerId structural CHECK is a single
    ///   comparison (`<1 µs` per row). The RanjId CHECK extracts two hex
    ///   digits from the canonical UUID text (`uuid::text` cast + two
    ///   `substring` calls) and runs in ~1–3 µs per row. Both are opt-in
    ///   because the **default-on** semantics would break every existing
    ///   model that holds an ID generated outside HeeRanjID (raw SQL
    ///   migrations, BI-tool writes, sister apps).
    ///   **Combination with adopter `#[field(check = "...")]`.** When the
    ///   adopter also declares a CHECK on a strict-ID-checked column, the
    ///   projection combines all three (strict-ID + type-derived +
    ///   adopter) into the single constraint slot via logical `AND`,
    ///   mirroring the existing AND-merge contract.
    ///   **Migration to Route B (centralized HeeRanjID validator).** The
    ///   CHECK projected here lives inside djogi and tracks HeeRanjID's
    ///   bit layout (verified against `~/projects/HeeRanjID/heeranjid/src`
    ///   at the time this attribute landed). A future HeeRanjID release
    ///   will ship `IMMUTABLE PARALLEL SAFE` Postgres validator functions
    ///   (`heeranjid.is_valid_heerid(BIGINT)` /
    ///   `heeranjid.is_valid_ranjid(UUID)`); when those land, djogi will
    ///   migrate to projecting `CHECK (heeranjid.is_valid_heerid(<col>))`
    ///   against them so the validator becomes a single source of truth.
    ///   The opt-in attribute surface stays unchanged across that
    ///   migration. See `docs/spec/decisions.md` "HeerId / RanjId
    ///   structural CHECK" for the route-A / route-B trade.
    ///   .
    pub strict_id_check: bool,

    /// Adopter-supplied `#[field(type_change_using = "<sql expr>")]` — Phase
    /// 8.5 .
    /// **One-time migration directive.** When the migration differ detects
    /// that this column's `sql_type` changed between the prior snapshot and
    /// the current descriptor, the SQL emitter appends the adopter's
    /// `USING (<expr>)` clause to the emitted
    /// `ALTER TABLE … ALTER COLUMN … TYPE …` statement. The clause unlocks
    /// non-default casts (e.g. `TEXT → UUID`, `TEXT → INTEGER`, domain
    /// changes) that Postgres refuses to apply automatically.
    /// **Raw SQL escape.** The expression is treated identically to a raw
    /// SQL fragment — djogi performs no parsing, no sanitisation, and no
    /// semantic validation beyond rejecting empty / whitespace-only strings
    /// at macro-parse time. Adopters are responsible for the expression's
    /// correctness against the column's old and new types; a wrong
    /// expression can silently corrupt or truncate column data. The same
    /// `unsafe`-style cultural posture from
    /// `docs/spec/raw-sql-escape-hatches.md` applies — every callsite should
    /// be reviewable as raw SQL.
    /// **Lifetime.** The attribute is a *one-time directive* attached to the
    /// migration that performs the type change. The slot is intentionally
    /// excluded from the persisted snapshot (`#[serde(skip)]` on the
    /// corresponding `ColumnSchema::type_change_using` field), so leaving
    /// the attribute in source after the migration applies does not produce
    /// a phantom diff — the next compose run sees the same `sql_type` on
    /// both sides, the differ emits no `ChangeType`, and the attribute
    /// stays dormant. Adopters are encouraged (but not required) to remove
    /// the attribute after the migration is applied; see
    /// `docs/guide/models.md` for the recommended workflow.
    /// **No down-side USING expression.** The emitter renders the default
    /// `USING <col>::<old_type>` cast on the down (rollback) side. Adopters
    /// whose rollback requires its own non-default cast hand-edit the
    /// emitted down SQL — symmetric down-side `using` expressions are not
    /// modelled here because the rollback path is operator-owned in
    /// practice.
    pub type_change_using: Option<&'static str>,
}

/// Adopter-supplied override for the Postgres volatility class of a
/// column default expression. Descriptor surface; the live-migration
/// classifier consumes the override.
/// Variants mirror Postgres's `provolatile` categories:
/// - [`Self::Immutable`] — the expression always returns the same value
///   for the same inputs and never reads database state. Safe to evaluate
///   once and cache.
/// - [`Self::Stable`] — the expression returns the same value within a
///   single query / statement but can vary across statements (e.g.
///   `now()` is STABLE, not VOLATILE).
/// - [`Self::Volatile`] — the expression can return different values on
///   each call (e.g. `clock_timestamp()`, `random()`). Triggers the
///   3-step ExpandContract pattern at compose time.
///   Narrow constructor for [`FieldDescriptor`] — required identity
///   fields at call site, every optional field defaulted.
/// # Why this exists
/// New `FieldDescriptor` fields land roughly once per phase. Without
/// this constructor, every test fixture that constructs a literal
/// must be updated whenever a new field appears — easy to forget a
/// site, easy to copy-paste-update incorrectly. With this constructor,
/// new fields plug in here once and every call site that uses
/// `..field_descriptor(...)` spread absorbs the change for free.
/// # Why required args, not an `EMPTY` const
/// An `EMPTY` const with blank `name` and a placeholder `sql_type`
/// would sanctify invalid descriptor identity — a fixture that forgets
/// to override `name` would silently produce a bogus column. The
/// required-args form forces semantic identity at call time and lets
/// the compiler catch missing fields.
/// # Use site
/// ```ignore
/// FieldDescriptor {
/// unique: true,
/// ..field_descriptor("email", FieldSqlType::Text, false)
/// }
/// ```
pub const fn field_descriptor(
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
        is_self_fk: false,
        visage_map: &[],
        protected: None,
        default_volatility_override: None,
        generated: None,
        // 5 — composition-derive provenance defaults to
        // `None`. Composition-derive emitters (Auditable / SoftDeletable)
        // override this on the specific contributed columns.
        composed_via: None,
        // — source-type discriminator.
        // Defaults to `None` (direct bind — no shim needed). The macro
        // sets `Some(RustSourceType::*)` for i8 / u8 / u16 / u32 / u64.
        rust_source_type: None,
        // — adopter `#[field(check)]`
        // raw-SQL CHECK expression. Defaults to `None` (no adopter
        // check); the macro fills `Some(<expr>)` from the attribute.
        check_sql: None,
        // Djogi#217) — adopter
        // `#[field(comment = "…")]` column-level free-text comment
        // lowered to `COMMENT ON COLUMN <t>.<c> IS '<text>'`. Defaults
        // to `None` (no comment); the macro fills `Some(<text>)` from
        // the attribute.
        comment: None,
        // — opt-in structural CHECK
        // for HeerId / RanjId columns. Defaults to `false` (no extra
        // CHECK; matches pre-189 behaviour). The macro sets `true`
        // for fields opted in via `#[field(strict_id_check)]` or
        // `#[model(strict_ids)]`; the projection layer reads this
        // flag plus the column's HeerRanjID semantic family
        // (`StrictIdFamily` — derived from the parent `PkType` for the
        // framework `id` column and from the FK target's `PkType` for
        // relation columns) to decide which structural CHECK to emit
        // (HeerId family → `col >= 0`; RanjId family → UUIDv8 + RFC 4122;
        // any other family — Serial, Custom, Composite, None — no
        // CHECK). Custom PKs whose inner SQL_TYPE coincidentally matches
        // BIGINT / UUID never inherit a HeerRanjID CHECK; the family
        // dispatch correctly maps them to `None`.
        strict_id_check: false,
        // Djogi#220) — adopter
        // `#[field(type_change_using = "<expr>")]` USING clause for
        // non-default-cast column type changes. `None` for every field
        // that does not opt in. The macro stores the verbatim
        // adopter-supplied SQL fragment; the projection threads it
        // into `ColumnSchema::type_change_using` (transient slot,
        // `#[serde(skip)]`); the differ pulls it into
        // `ColumnChange::ChangeType::using` only when the column's
        // `sql_type` actually changed. Leaving the attribute on a
        // field after applying produces no diff because the type
        // comparison itself is what gates emission.
        type_change_using: None,
    }
}

/// `#[non_exhaustive]` so future Postgres categories (or Djogi-specific
/// refinements) can land without breaking downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DefaultVolatility {
    /// Postgres `IMMUTABLE` — pure function, no side effects, no DB reads.
    Immutable,
    /// Postgres `STABLE` — consistent within one statement; consults DB
    /// state but does not modify it.
    Stable,
    /// Postgres `VOLATILE` — value can change on every call. Triggers
    /// the conservative live-migration path at compose time.
    Volatile,
}

impl Default for DefaultVolatility {
    /// `Volatile` is the conservative default — when the classifier
    /// cannot prove an expression is `Stable` or `Immutable`, it must
    /// route through the 3-step ExpandContract pattern rather than
    /// the catalog-only fast-path. Mirrors §7 of the v3 plan: "unknown
    /// identifiers default conservatively to VOLATILE."
    fn default() -> Self {
        Self::Volatile
    }
}

/// Sensitivity classification for protected fields. .
/// Drives downstream policy: redaction (logs / admin UI), codec
/// selection (at-rest encryption), retention (purge cadence). Ordered
/// from least to most sensitive — `None` is the explicit "ordinary
/// field" marker. Logging consumers compare ordinals to gate field
/// exposure, so the variant order is part of the contract.
/// `#[non_exhaustive]` so future regulatory classes (e.g. PHI / payment
/// data) can be added without breaking downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Sensitivity {
    /// Ordinary field; no protected-field semantics.
    None,
    /// Internal-only — operational metadata that should not surface
    /// in adopter-facing UIs by default.
    Internal,
    /// Personally identifying information (GDPR Art. 4(1)).
    Pii,
    /// Sensitive personal data (GDPR Art. 9 special categories) or
    /// equivalent regulatory class.
    Sensitive,
    /// Cryptographic secrets / authentication material.
    Secret,
}

impl Default for Sensitivity {
    /// `None` — opting into protected-field semantics is explicit.
    fn default() -> Self {
        Sensitivity::None
    }
}

/// Redaction policy applied when a protected field is rendered into
/// logs, admin UI, or any downstream surface that the adopter has not
/// explicitly opted into. .
/// `#[non_exhaustive]` so future strategies (tokenisation, format-
/// preserving redaction, etc.) can be added without breaking matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RedactionPolicy {
    /// No redaction — the value flows through verbatim.
    None,
    /// Hash the value using HeeRanjId-compatible hashing (HMAC-SHA256
    /// with a per-deployment key) before emission. Only valid on
    /// fields whose stored type is HeerId / RanjId compatible — the
    /// macro enforces that constraint at parse time.
    HashId,
    /// Mask the value — preserve length / type signal, replace
    /// content with the deployment's masking glyph (default `*`).
    Mask,
    /// Drop the field entirely from the redacted projection.
    Drop,
}

impl Default for RedactionPolicy {
    /// `None` — redaction is opt-in alongside the rest of the
    /// protected-field metadata.
    fn default() -> Self {
        RedactionPolicy::None
    }
}

/// Retention label drives later purge / archival policy. .
/// `Standard` is the neutral default — the project's default retention
/// window applies unless an adopter calls out a different label.
/// `#[non_exhaustive]` so future labels (e.g. legal-hold subclasses)
/// can be added without breaking downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RetentionLabel {
    /// Transient — purge eligible immediately after transactional use.
    Transient,
    /// Standard — purge per the project's default retention window.
    Standard,
    /// Extended — held longer than standard; specific window set by
    /// the adopter's retention policy.
    Extended,
    /// Archival — long-term retention for legal / regulatory holds.
    Archival,
}

impl Default for RetentionLabel {
    /// `Standard` — most fields fall under the default retention
    /// window; transient / extended / archival are explicit choices.
    fn default() -> Self {
        RetentionLabel::Standard
    }
}

/// Protected-field metadata attached to a [`FieldDescriptor`]. .
/// Attached as `Option<ProtectedFieldMetadata>` on `FieldDescriptor`;
/// `None` means "no protected-field semantics set" (the absence is
/// distinct from `Sensitivity::None` because absence also implies the
/// adopter never invoked `#[field(protected(...))]`, which keeps later
/// auditing tools from confusing the two).
/// Fields are transport-neutral — every consumer (classifier, logging
/// projection, admin UI projection) reads the same struct without
/// reinterpretation.
/// `rationale` is a bare `&'static str` (not `Option<&'static str>`)
/// to match the spec: when `sensitivity == None` the rationale is
/// empty (`""`); when `sensitivity > None` the macro enforces a
/// non-empty literal at parse time. The descriptor itself does not
/// enforce the constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtectedFieldMetadata {
    pub sensitivity: Sensitivity,
    pub rationale: &'static str,
    pub redaction: RedactionPolicy,
    pub codec: Option<&'static str>,
    pub retention: RetentionLabel,
}

impl Default for ProtectedFieldMetadata {
    /// Neutral defaults: no sensitivity, no rationale, no redaction,
    /// no codec, standard retention. Constructed via `..Default::default()`
    /// when the macro elides unset attribute knobs.
    fn default() -> Self {
        Self {
            sensitivity: Sensitivity::None,
            rationale: "",
            redaction: RedactionPolicy::None,
            codec: None,
            retention: RetentionLabel::Standard,
        }
    }
}

/// `GENERATED ALWAYS AS (<expression>) STORED` column metadata.
/// PR 7 — Postgres 18 stored generated columns. Used by
/// fields whose value is computed from other columns at write time
/// rather than being supplied by the application. The expression is
/// stored verbatim and emitted into the column DDL inside
/// `GENERATED ALWAYS AS (<expression>) STORED`.
/// # Classification
/// Adding or changing a stored generated column's expression on a
/// **populated** table classifies as `OfflineOnly`: the replacement
/// column itself rewrites the table, so a shadow-column expand/
/// contract pattern offers no relief. The empty-table case (CREATE
/// TABLE, or an existing table with zero rows) emits the column
/// inline.
/// # Pg version note
/// Pg18 only supports `STORED`. `VIRTUAL` columns land in Pg19+;
/// `stored: false` is reserved for that future variant and currently
/// rejected by the macro at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GeneratedColumnSpec {
    /// SQL expression used to compute the column value. Emitted
    /// verbatim inside `GENERATED ALWAYS AS (<expression>) STORED`.
    /// Plain column references like `"LOWER(email)"` and arbitrary
    /// expressions are both supported — the descriptor does not
    /// validate or rewrite the SQL.
    pub expression: &'static str,
    /// `true` emits `STORED`. Pg18 supports only stored generated
    /// columns; `false` is reserved for the future Pg19+ `VIRTUAL`
    /// variant and is rejected by the macro today.
    pub stored: bool,
}

/// Sidecar FK-deferrability metadata emitted separately from
/// [`FieldDescriptor`] so hand-written descriptor literals remain
/// source-compatible when new deferrability knobs are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferrabilitySpec {
    pub model_type_name: &'static str,
    pub field_name: &'static str,
    pub deferrable: bool,
    pub initially_deferred: bool,
}

/// Describes an adopter-declared custom PK type.
/// Named struct (not bare enum-variant fields) so future fields can be
/// added without rewriting every `PkType::Custom` match arm in the
/// codebase. Added in alongside the
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
/// The six leaf variants (`HeerId`, `RanjId`, `HeerIdDesc`, `RanjIdDesc`,
/// `Serial`, `None`) map 1:1 to the `#[model(pk = X)]` attribute identifiers
/// (`HeerId`, `RanjId`, `HeerIdRecencyBiased` | `HeerIdDesc`,
/// `RanjIdRecencyBiased` | `RanjIdDesc`, `Serial`, `None`).
/// `Composite` is emitted for models that declare multiple PK columns
/// rare, mostly join tables — and carries the ordered list of column names.
/// `Custom` is emitted for adopter-declared PK types registered through
/// [`PrimaryKey`](crate::primary_key::PrimaryKey) + `djogi::primary_key!`
/// (the variant exists so match sites
/// across the crate freeze their exhaustiveness contract now).
/// `HeerIdDesc` / `RanjIdDesc` (added in v3) store the same
/// logical identity as their ascending siblings but with timestamp + sequence
/// bits XORed so that a BTree index on the PK column scans
/// **most-recent-first** without a secondary descending index. See
/// [`crate::types::HeerIdDesc`] / [`crate::types::RanjIdDesc`] and the
/// Plan §4.1 for the full indexing trade-off. The ascending ↔
/// descending PK migration itself lands in a later release; this release only freezes the
/// variant additions, attribute-parse paths, and descriptor shape.
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
/// This is the single source of truth for every framework subsystem that
/// reflects on schema: migrations, `djogi docs`, RLS
/// generation, partitioning, the outbox system
/// , visages (— `expose(...)` scope membership),
/// protected-data governance (— sensitivity, codecs, redaction),
/// data-lifecycle planning, and distributed topology
/// (shard/residency/placement metadata). Extending this struct
/// is a coordinated action across all call sites — every
/// `inventory::submit!` generated by the macro must fill every field.
/// `fields` contains the **complete** column set: framework-injected columns
/// (`id`, `created_at`, `updated_at`) appear before user-declared fields in
/// injection order. See [`FieldDescriptor`] for the exact ordering contract.
/// `Clone` was added in because the migrate module's
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
    /// `query_insecurely` bypasses the check .
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
    /// `partition_by` is set without a rationale .
    pub rationale: Option<&'static str>,

    // ── Indexes (items 43–46, graph-spatial-advanced-indexing plan) ─────────
    /// Named index declarations. emits an empty slice by default.
    /// Migration generation for these lands in .
    pub indexes: &'static [IndexSpec],

    // ── Many-to-many (Task 6, phase3-relations plan) ────────────────────────
    /// `true` when the model is a `#[model(table = "...", through)]`
    /// junction table for a specific `impl ManyToMany<Target> for Source`.
    /// Through models remain ordinary queryable `Model`s — this flag is
    /// purely a marker carried in the descriptor for downstream consumers:
    /// - the migration differ can suppress standalone admin /
    ///   routing affordances for through tables (deferred).
    /// - Human-facing tools (`djogi docs`, the shell's `.list_models`)
    ///   can hide through tables from the primary model list.
    ///   `#[derive(Model)]` without `through` sets this to `false`.
    pub is_through: bool,

    // ── Full-Text Search ──────────────────────────────────
    /// Full-text search configuration when `#[model(fts = { source = "...",
    /// dictionary = "..." })]` is set.
    /// `None` for every model that does not declare an FTS column (the
    /// default). `Some(spec)` means a `GENERATED ALWAYS AS` tsvector column
    /// is expected in the schema and a GIN index should accompany it.
    /// # migration differ — important note
    /// **Changing `FtsDescriptor.dictionary` is a column-type alteration.**
    /// The generated column expression embeds the dictionary name literally:
    /// `to_tsvector('<dictionary>', <source>)`. Altering the dictionary
    /// requires dropping and re-creating the generated column — the migration
    /// differ must treat this field the same way it treats a `FieldSqlType`
    /// change (drop + add, not an in-place ALTER). Differ authors: compare
    /// `old_desc.fts` with `new_desc.fts` using `PartialEq` — any difference
    /// in `column`, `source`, or `dictionary` requires a column reconstruction.
    pub fts: Option<FtsDescriptor>,

    // ── Apps subsystem ─────────────────────────────────
    /// The stable string label of the app this model belongs to.
    /// Set by `#[model(app = Vehicles)]` — the macro lowers the type
    /// path to `<Vehicles as ::djogi::App>::LABEL` at const-eval time
    /// so the descriptor carries the Postgres identifier, not the
    /// Rust type name. `None` places the model in the synthetic
    /// global bucket, which the differ files under
    /// `<default-database>/<empty-label>/`.
    pub app: Option<&'static str>,

    /// Historical-metadata pointer to this model's prior app.
    /// Set by `#[model(moved_from_app = OldBilling)]` when a model
    /// has been moved between apps. Enables the migration
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
    /// Carries the old **string** table name, not a type — old
    /// model types may no longer exist in source after a sweep that
    /// renamed many tables in one pass.
    /// `None` for every model that has not been renamed (the common
    /// case).
    pub renamed_from: Option<&'static str>,

    // ── EXCLUDE constraints (PR 7) ────────────────────────────────
    /// `EXCLUDE` constraint declarations on this model. Empty slice
    /// the default — means no exclusion constraints. the
    /// migration differ emits `ALTER TABLE … ADD CONSTRAINT …
    /// EXCLUDE …` for each entry on table create / empty-table; on
    /// populated tables the live-migration classifier routes
    /// additions to `OfflineOnly`.
    pub exclusion_constraints: &'static [ExclusionConstraintSpec],

    // ── Tree queries ────────────────────────
    /// Default self-FK column for tree-recursive queries. Set via
    /// `#[model(tree_edge = "parent_id")]`.
    /// When `Some(col)`, B2's `T::tree_descendants(root_id)` inherent
    /// uses this column as the parent edge without requiring an
    /// explicit `RelationPath` argument. The macro validates at
    /// expand time that the named field exists on the struct AND
    /// resolves to a self-FK via [`FieldDescriptor::is_self_fk`];
    /// failures surface as span-precise compile errors.
    /// `None` means the model has no default tree edge — callers
    /// must pass `RelationPath` explicitly to the recursive-query
    /// builder. Models with two or more self-FK edges and no
    /// `tree_edge` annotation are valid descriptors but force the
    /// caller into the explicit-path form (B5 covers the lihaaf
    /// compile-fixture under `djogi-macros/tests/compile_fail/`).
    pub tree_edge: Option<&'static str>,

    // ── Proxy models ──────────────────────────────
    /// `Some("Vehicle")` when this model is a proxy of `Vehicle`
    /// shares its parent's table, has its own Rust type. .
    /// Migration differ treats proxies as schema-passthrough: no
    /// `CREATE TABLE` emitted; the parent owns the table. Proxies
    /// can override the `Model` trait surface (`default_filter`,
    /// `default_order_by`, type-specific hooks) without breaking the
    /// parent's storage layout.
    /// `None` for ordinary (non-proxy) models — the common case.
    pub proxy_for: Option<&'static str>,

    /// SQL fragment for the proxy's default filter, if any. Macro-
    /// emitted as a constant string at expand time. Composed into
    /// every `QuerySet<ProxyModel>` via the `Model::default_filter_condition`
    /// override. The fragment is the lowered form of the
    /// `default_filter = |f| ...` closure on `#[model(...)]`.
    /// `None` for non-proxy models and for proxies without a
    /// `default_filter` clause.
    pub default_filter_sql: Option<&'static str>,

    // ── Computed properties ───────────────────────
    /// Computed (virtual / SQL-projectable) fields declared via
    /// `#[computed(sql = "...")]` on a struct field.
    /// Each entry records the field's Rust name, the SQL expression
    /// source, and the SQL-side return type. Migration differ does NOT
    /// consult these in v0.1.0 (computed fields are non-stored
    /// evaluated at query time, not persisted as columns); the
    /// descriptor entry exists so `djogi docs` can render
    /// computed-field documentation alongside regular columns and so
    /// the macro emitter can cross-reference computed names against
    /// regular field names for collision detection.
    /// `&[]` for models without any `#[computed]` fields — the common
    /// case. `&'static [...]` rather than `Vec` to keep the descriptor
    /// `inventory`-submittable with no allocation at registration.
    pub computed_fields: &'static [ComputedFieldDescriptor],

    // ── DDL metadata ─────────────────────────
    /// Adopter-supplied `#[model(table_comment = "<text>")]` free-text
    /// table comment, lowered by the migration composer to
    /// `COMMENT ON TABLE <table> IS '<text>'` immediately after the
    /// `CREATE TABLE` statement. The differ surfaces table-comment
    /// changes via
    /// [`crate::migrate::diff::SchemaOperation::SetTableComment`],
    /// which lowers to a single `COMMENT ON TABLE` (or `... IS NULL`
    /// when the comment is dropped).
    /// `None` for models that declare no table-level comment — the
    /// common case.
    /// **Quoting.** Same contract as [`FieldDescriptor::comment`]:
    /// adopter prose stored verbatim, composer doubles apostrophes at
    /// SQL-emission time.
    /// Djogi#217).
    pub table_comment: Option<&'static str>,

    /// Adopter-supplied `#[model(storage_params = "key=val, ...")]`
    /// per-table storage-parameter list. Lowered by the migration
    /// composer to `ALTER TABLE <table> SET (key=val, ...)`.
    /// The macro stores a canonical safe `key=value` fragment after
    /// parsing and validating structured entries. The SQL emitter
    /// re-parses the fragment and renders `SET` / `RESET` from those
    /// entries instead of splicing caller-provided text into SQL.
    /// Djogi#218).
    pub storage_params: Option<&'static str>,

    /// Adopter-supplied `#[model(tablespace = "<name>")]` target
    /// tablespace. Lowered by the migration composer to
    /// `ALTER TABLE <table> SET TABLESPACE <name>` after table
    /// creation or when the snapshot value changes.
    /// Validated at macro parse time as a plain Postgres identifier and
    /// quoted by the SQL emitter. `None` means no explicit tablespace
    /// (the database default).
    /// Djogi#219).
    pub tablespace: Option<&'static str>,
}

/// Descriptor for one `#[computed(sql = "...")]` field.
/// Computed fields are non-stored in v0.1.0: the SQL expression is
/// evaluated at query time (filter, order_by, annotate via the
/// `{Model}Computed` ZST emitted alongside the model), and the Rust-
/// side getter evaluates the equivalent expression in-memory for
/// adopters who need to call `vehicle.total_price()` after `fetch_one()`
/// without a re-query. The stored variant (`#[computed(sql, stored)]`)
/// is deferred to — the macro emits a span-precise deferral
/// error when the `stored` keyword appears.
/// # Field stability
/// Like [`FieldDescriptor`] and [`ModelDescriptor`], this struct uses
/// `&'static str` for every text field so the descriptor literal
/// emitted by the macro is `inventory`-submittable. Adding a field is
/// a breaking change for every existing `ComputedFieldDescriptor`
/// literal site — only do it when load-bearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedFieldDescriptor {
    /// Rust identifier for the computed field — also the suggested
    /// `SELECT`-list alias when the expression is used in
    /// `.annotate()`. Validated at parse time against the same
    /// byte-level grammar as regular column names: ASCII letter or
    /// underscore start, alphanumerics or underscores after, ≤ 63
    /// bytes (Postgres unquoted-identifier cap).
    pub name: &'static str,

    /// SQL expression source as written in the `#[computed(sql = "...")]`
    /// attribute. Used by the `djogi docs` to render the
    /// computed-field documentation alongside regular columns;
    /// consumed by the macro-emitted `{Model}Computed` ZST at filter /
    /// order_by / annotate time to splice the fragment into the
    /// emitted SQL. The `&'static str` is verbatim — no transformation
    /// happens at descriptor-emission time.
    pub sql: &'static str,

    /// SQL-side return type, lowered from the Rust return type of the
    /// computed getter. The migration differ does NOT consult this in
    /// v0.1.0 (computed fields are non-stored), so it is documentation-
    /// only at this phase. Future work may use it to validate the
    /// computed expression's SQL type against adopter-declared casts.
    pub value_type: FieldSqlType,
}

impl ModelDescriptor {
    /// Returns `true` if this model has a GiST index on at least one
    /// `Geography`-typed field.
    /// Used by [`crate::query::queryset::QuerySet::group_by_region`] at
    /// call time to warn when the region model has no spatial index, which
    /// causes the spatial JOIN to scan the region table linearly for every
    /// row in the data table.
    /// # Detection algorithm
    /// For each [`IndexSpec`] in `self.indexes`:
    /// 1. Skip entries whose `index_type` is not [`IndexType::Gist`].
    /// 2. Skip entries whose `target` is [`IndexTarget::Expression`]
    ///    expression-target spatial indexes are legal but their column
    ///    relationship is opaque here, so we conservatively answer "no"
    ///    rather than guess.
    /// 3. For each [`IndexColumnSpec`] in the spec's column list, check
    ///    whether the corresponding [`FieldDescriptor`] has
    ///    `sql_type == FieldSqlType::Geography { .. }`.
    /// 4. Return `true` as soon as one such matching field is found.
    ///    Composite indexes count if **any** column in the index is
    ///    `Geography`-typed. This reflects Postgres's GiST-prefix behaviour:
    ///    a GiST index on `(boundary, other_col)` is still a valid spatial
    ///    index that accelerates `ST_Contains` / `ST_DWithin` lookups on
    ///    `boundary`, so we treat such composite indexes as satisfying the
    ///    "has GiST on geography" check.
    ///    Returns `false` if no GiST + Geography combination is found.
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

    /// Iterate every self-FK [`FieldDescriptor`] on this model — the
    /// shared filter predicate behind [`Self::self_fk_count`] and
    /// [`Self::self_fk_columns`]. Single source of truth for "what
    /// counts as a self-FK edge" in case a future field rename or
    /// the migration differ acquires a third consumer.
    fn self_fk_fields(&self) -> impl Iterator<Item = &FieldDescriptor> + '_ {
        self.fields
            .iter()
            .filter(|f| f.relation_kind.is_some() && f.is_self_fk)
    }

    /// Count of self-FK edges on this model — the number of fields
    /// whose `relation_kind` is `Some(_)` and whose `is_self_fk`
    /// flag is `true`.
    /// Used by the recursive-query builder:
    /// - `0` — `T::tree_descendants(...)` is unavailable; caller must
    ///   declare a self-FK before reaching for the tree-query API.
    /// - `1` — exactly one parent edge; the inherent sugar resolves
    ///   the column without ambiguity.
    /// - `2+` — multiple self-FK edges; the model must declare
    ///   `#[model(tree_edge = "...")]` to disambiguate, or the
    ///   caller must pass an explicit `RelationPath<T, T>` argument.
    pub fn self_fk_count(&self) -> usize {
        self.self_fk_fields().count()
    }

    /// Iterate the column names of every self-FK edge declared on this
    /// model — every field whose `relation_kind` is `Some(_)` and whose
    /// `is_self_fk` flag is `true`.
    /// Used by [`Model::full_ancestors`](crate::model::Model::full_ancestors)
    /// to discover every parent edge to walk in one recursive CTE — each
    /// returned column name becomes the source-column of a synthesised
    /// `RelationPath<T, T>` and an alternative inside the lateral
    /// `UNION ALL` subquery the recursive term joins to.
    /// Returns an empty iterator when `self_fk_count() == 0`. Order
    /// matches descriptor field-injection order, which equals struct
    /// declaration order for user-defined columns — so emitted SQL is
    /// stable across builds for a fixed source.
    pub fn self_fk_columns(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.self_fk_fields().map(|f| f.name)
    }

    /// The primary key column name for this model.
    /// Returns `Some("id")` for the five standard PK types (`HeerId`,
    /// `RanjId`, `HeerIdDesc`, `RanjIdDesc`, `Serial`) and `None` for
    /// `pk = None` models. `Composite` PKs are uncommon; this method
    /// returns the first column in the composite list on the assumption
    /// that it is the most natural tiebreak candidate.
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
    /// Returns a [`migration_shape::MigrationShape`] that captures every
    /// DDL decision the descriptor encodes — column SQL types, index DDL
    /// (including `CONCURRENTLY` placement for out-of-transaction indexes),
    /// and the set of Postgres extensions that must be present.
    /// This is a **contract helper**, not a runtime path. will
    /// subsume it by emitting the same shape's content as actual migration
    /// SQL files. Until then, contract tests assert against this structure
    /// to prove the descriptor encodes enough information for a downstream
    /// emitter to produce correct DDL without type-name inference.
    /// Visibility is `pub` so feature-gated integration tests can call it;
    /// May narrow it back to `pub(crate)` once the emitter owns this
    /// responsibility.
    pub fn migration_shape(&self) -> migration_shape::MigrationShape {
        migration_shape::MigrationShape::from_descriptor(self)
    }
}

/// Narrow constructor for [`ModelDescriptor`] — required identity
/// fields at call site, every optional field defaulted.
/// # Why this exists
/// `ModelDescriptor` accumulates one or two new fields per phase.
/// Without this constructor, every test fixture and inline
/// `impl Model for X` block that constructs a literal must be
/// updated whenever a new field appears. With this constructor, new
/// fields plug in here once and every call site that uses
/// `..model_descriptor(...)` spread absorbs the change for free.
/// # Why required args, not an `EMPTY` const
/// An `EMPTY` const with blank `type_name` / `table_name` would
/// sanctify invalid descriptor identity — a fixture that forgets to
/// override `table_name` would silently produce a descriptor with
/// an empty table reference, which `MigrationShape::from_descriptor`
/// would then propagate into bogus DDL. The required-args form
/// forces semantic identity at call time and lets the compiler catch
/// missing fields.
/// # Use site
/// ```ignore
/// ModelDescriptor {
/// tenant_key: Some("org_id"),
/// ..model_descriptor("Widget", "widgets", PkType::HeerId, FIELDS)
/// }
/// ```
pub const fn model_descriptor(
    type_name: &'static str,
    table_name: &'static str,
    pk_type: PkType,
    fields: &'static [FieldDescriptor],
) -> ModelDescriptor {
    ModelDescriptor {
        type_name,
        table_name,
        pk_type,
        fields,
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
        exclusion_constraints: &[],
        tree_edge: None,
        // Proxy models (schema-passthrough). Defaults to `None`;
        // populated by the macro when `#[model(proxy_for = ParentType)]` is
        // present.
        proxy_for: None,
        default_filter_sql: None,
        // Computed (virtual / SQL-projectable) fields.
        // Defaults to the empty slice; populated by the macro when one or
        // more struct fields carry `#[computed(sql = "...")]`.
        computed_fields: &[],
        // Djogi#217) — adopter
        // `#[model(table_comment = "…")]` lowered to
        // `COMMENT ON TABLE`. Defaults to `None` (no comment); the
        // macro fills `Some(<text>)` from the attribute.
        table_comment: None,
        // Djogi#218) — adopter
        // `#[model(storage_params = "key=val, ...")]` lowered to
        // `ALTER TABLE ... SET (...)`. Defaults to `None`.
        storage_params: None,
        // Djogi#219) — adopter
        // `#[model(tablespace = "...")]` lowered to `ALTER TABLE ...
        // SET TABLESPACE`. Defaults to `None`.
        tablespace: None,
    }
}

inventory::collect!(ModelDescriptor);
inventory::collect!(DeferrabilitySpec);

/// Contract-validation helper that maps a [`ModelDescriptor`] to the DDL
/// intent it implies.
/// # Why this module exists
/// Will emit actual `.sql` migration files. Before lands,
/// this module proves the descriptor already encodes *all* information the
/// emitter will need: column SQL types (including PostGIS `GEOGRAPHY`),
/// per-index CONCURRENTLY placement, required Postgres extensions
/// (including `btree_gist` for exclusion constraints under ), and
/// exclusion-constraint shape. Contract tests assert against
/// [`MigrationShape`] values constructed from macro-emitted
/// `ModelDescriptor`s.
/// # Placement decision
/// The content is under 150 lines so it lives as an in-file submodule of
/// `descriptor.rs` rather than a sibling `descriptor/migration_shape.rs`
/// file. This avoids a directory-split refactor and keeps the contract
/// helper adjacent to the types it describes.
pub mod migration_shape {
    use std::collections::BTreeSet;

    use super::{
        ExclusionConstraintSpec, FieldSqlType, GeneratedColumnSpec, IndexKind, IndexTarget,
        IndexType, ModelDescriptor,
    };

    // -----------------------------------------------------------------------
    // Public types
    // -----------------------------------------------------------------------

    /// The migration-SQL intent implied by a [`ModelDescriptor`].
    /// Contract helper — will be subsumed by emitting the same
    /// structure's contents as actual DDL files. Until then,
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
        /// DDL runs. Collected from:
        /// - every `IndexSpec::extension_dependency` that is `Some`
        /// - every field whose `sql_type` is `FieldSqlType::Geography`
        ///   (even if no index exists — the column itself requires PostGIS)
        /// - every `ExclusionConstraintSpec::extension_dependency` that
        ///   is `Some` (— typically `btree_gist` for gist
        ///   EXCLUDEs that mix `=` with `&&` overlap operators)
        pub required_extensions: BTreeSet<&'static str>,
        /// One entry per [`ExclusionConstraintSpec`] in
        /// `ModelDescriptor::exclusion_constraints` (PR 7).
        /// The descriptor's exclusion specs are already small and
        /// `Copy`-cheap, so the projection stores values directly
        /// rather than references — same shape the IndexShape vec uses.
        pub exclusion_constraints: Vec<ExclusionConstraintSpec>,
    }

    /// DDL-relevant metadata for a single column.
    #[derive(Debug, Clone)]
    pub struct ColumnShape {
        /// Column name from `FieldDescriptor::name`.
        pub name: &'static str,
        /// SQL type string produced by `FieldSqlType`'s `Display` impl.
        /// Case matches the `Display` impl exactly:
        /// - Standard types are uppercased (`"TEXT"`, `"BIGINT"`, `"TIMESTAMPTZ"`).
        /// - `Geography { srid }` is lowercase-prefixed:
        ///   `"geography(Point, 4326)"`.
        ///   The plan's prose example used `"GEOGRAPHY(Point, 4326)"` (uppercase
        ///   prefix) as an illustration. The actual `Display` impl uses lowercase
        ///   `"geography(Point, 4326)"`. Contract tests follow the Display impl
        ///   keeping one canonical text path is more important than matching the
        ///   prose example's capitalisation.
        pub sql_type_text: String,
        /// `true` when `FieldDescriptor::nullable` is `false` (the column is
        /// `NOT NULL` in SQL).
        pub not_null: bool,
        /// Mirrors `FieldDescriptor::generated` — when `Some`, the
        /// emitter appends `GENERATED ALWAYS AS (<expression>) STORED`
        /// to the column DDL (PR 7).
        pub generated: Option<GeneratedColumnSpec>,
    }

    /// DDL-relevant metadata for a single index, plus the SQL the emitter
    /// would produce.
    /// This is a simplified projection of the `IndexSpec`
    /// tailored to the `MigrationShape` contract — the real
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
        /// MigrationShape contract does not distinguish the two forms
        /// The real differ reads `IndexSpec::kind` directly.
        pub unique: bool,
        /// Mirrors `IndexSpec::requires_out_of_transaction`.
        /// When `true`, `sql_text` will contain `CONCURRENTLY`.
        pub requires_out_of_transaction: bool,
        /// Mirrors `IndexSpec::extension_dependency`.
        pub extension_dependency: Option<&'static str>,
        /// The `CREATE INDEX` statement the emitter would produce.
        /// Not executed in this is the contract proof.
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
                    generated: f.generated,
                })
                .collect();

            // ── Exclusion constraints (PR 7) ─────────────────────
            let exclusion_constraints: Vec<ExclusionConstraintSpec> =
                desc.exclusion_constraints.to_vec();

            // ── Required extensions from fields ────────────────────────────
            let mut required_extensions: BTreeSet<&'static str> = BTreeSet::new();
            for f in desc.fields {
                if matches!(f.sql_type, FieldSqlType::Geography { .. }) {
                    required_extensions.insert("postgis");
                }
            }
            // — exclusion constraints carry their
            // own extension dependency (typically `btree_gist` for gist
            // EXCLUDEs that mix `=` with `&&` overlap operators). The
            // macro auto-derives the dependency so adopters do not have
            // to remember which extension a given EXCLUDE shape needs.
            for spec in desc.exclusion_constraints {
                if let Some(ext) = spec.extension_dependency {
                    required_extensions.insert(ext);
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
                    // V3: IndexSpec now carries
                    // `target: IndexTarget` (column list | expression) and
                    // `kind: IndexKind`. Collapse back to the
                    // `(columns: Vec<&str>, unique: bool)` shape the
                    // MigrationShape contract expects — the richer per-column
                    // knobs (opclass / order / nulls) stay on the underlying
                    // IndexSpec for the real differ to consume.
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
                exclusion_constraints,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Map an `IndexType` to its lowercase Postgres keyword.
    /// No `Display` impl is added to `IndexType` itself because the index
    /// keyword (`"gist"`) is a DDL-emission concern, not a general display
    /// concern. This helper is local to the migration-shape module.
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
/// `#[derive(DjogiEnum)]` emits one `inventory::submit!(EnumDescriptor { ... })` per enum.
/// The migration differ consumes these via `inventory::iter::<EnumDescriptor>` to
/// emit `CREATE TYPE <postgres_type> AS ENUM (...)` DDL statements.
/// # Layout
/// - `type_name` — Rust type name as a string (`"VehicleStatus"`). Used by the migration
///   differ and `djogi docs` to identify the origin type.
/// - `postgres_type` — Postgres type name from `#[djogi_enum(name = "...")]`
///   (`"vehicle_status"`). This is the value passed to `CREATE TYPE ... AS ENUM`.
/// - `variants` — mapped string labels in declaration order. These are the wire values that
///   appear in the Postgres `ENUM` definition and in every serialized row.
///   Owns DDL emission; only supplies the descriptor so the collector is
///   populated and ready for migration consumers.
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

/// Type-erased SQL bind value produced by a framework-owned runtime codec.
#[doc(hidden)]
pub type BoxedSqlBind = Box<dyn postgres_types::ToSql + Sync + Send>;

/// Binder for a single enum operand stored in Sassi's type-erased predicate payload.
#[doc(hidden)]
pub type EnumPredicateValueBinder =
    for<'a> fn(&'a (dyn std::any::Any + Send + Sync)) -> Option<BoxedSqlBind>;

/// Binder for a `Vec<Enum>` operand stored in Sassi's type-erased predicate payload.
#[doc(hidden)]
pub type EnumPredicateListBinder =
    for<'a> fn(&'a (dyn std::any::Any + Send + Sync)) -> Option<Vec<BoxedSqlBind>>;

/// Binder for an `Option<Enum>` operand stored in Sassi's type-erased predicate payload.
#[doc(hidden)]
pub type EnumPredicateOptionBinder =
    for<'a> fn(&'a (dyn std::any::Any + Send + Sync)) -> Option<Option<BoxedSqlBind>>;

/// Binder for a `Vec<Option<Enum>>` operand stored in Sassi's type-erased predicate payload.
#[doc(hidden)]
pub type EnumPredicateOptionListBinder =
    for<'a> fn(&'a (dyn std::any::Any + Send + Sync)) -> Option<Vec<Option<BoxedSqlBind>>>;

/// Runtime check that a model field's declared Rust type belongs to this enum codec.
#[doc(hidden)]
pub type EnumPredicateFieldTypeMatcher = fn(std::any::TypeId) -> bool;

/// Runtime predicate codec registered by `#[derive(DjogiEnum)]`.
/// The model macro cannot observe whether a sibling type also derived
/// `DjogiEnum` when it classifies a field. Instead, unknown/custom fields stay
/// conservative at macro expansion time and the portable SQL fallback consults
/// this registry at runtime. Only DjogiEnum-derived types submit entries, so
/// arbitrary adopter newtypes remain unsupported unless a future framework
/// macro registers an explicit codec for them.
#[doc(hidden)]
pub struct EnumPredicateCodec {
    pub type_name: &'static str,
    pub postgres_type: &'static str,
    pub matches_field_type: EnumPredicateFieldTypeMatcher,
    pub bind_value: EnumPredicateValueBinder,
    pub bind_list: EnumPredicateListBinder,
    pub bind_option_value: EnumPredicateOptionBinder,
    pub bind_option_list: EnumPredicateOptionListBinder,
}

inventory::collect!(EnumPredicateCodec);

// ───────────────────────────────────────────────────────────────────────────
// Visage-side derived-projection descriptor inventory — #231.
// Separate from `ModelDescriptor` (the storage-side inventory consumed by
// the migration differ). `inventory::collect!(VisageDescriptor)` registers
// its OWN collection, so the migration / snapshot / `build.rs` paths
// which iterate `inventory::iter::<ModelDescriptor>()` and
// `inventory::iter::<EnumDescriptor>()` exclusively — never observe
// derived projections. The boundary mirrors the storage-vs-projection
// split the rest of the visage-derived-fields surface establishes.
// ───────────────────────────────────────────────────────────────────────────

/// One derived projection entry — #231.
/// `#[derived(name, ty, scopes, sql, rust, doc)]` parses into this
/// shape at macro time, and the macro emits a fully-`&'static`-typed
/// literal inside an `inventory::submit!(VisageDescriptor { ... })`
/// block for the visages whose `scopes = [...]` list includes the
/// owning visage's scope.
/// # Why this exists separately from `ProjectionEntry`
/// [`crate::__private::ProjectionEntry`] is the **runtime metadata
/// enum** carried on every emitted visage's
/// [`crate::DjogiVisage::PROJECTIONS`] constant. It only carries the
/// alias + SQL fragment the queryset hot path needs.
/// `DerivedProjection` is the **descriptor record** intended for
/// out-of-band consumers — documentation generators, framework-side
/// lints, future Tier-2 predicate renderers. It carries the richer
/// shape (`ty_path`, `rust`, `doc`, originating `scopes`) those
/// consumers want without bloating the per-visage trait constant or
/// the SELECT-emission hot path.
/// # Const-construction contract
/// Every field is a type with a `const` constructor usable in static
/// contexts — `&'static str`, `Option<&'static str>`, and
/// `&'static [&'static str]`. The macro emits the entire
/// `&'static [DerivedProjection]` slice as a static-context expression
/// at the `inventory::submit!` site without runtime allocation. Owned
/// types (`String`, `Vec<T>`) are forbidden anywhere here because they
/// would force a runtime allocator and break `inventory::submit!`'s
/// static-data requirement; the same constraint binds
/// [`FieldDescriptor`].
/// # Field-stability contract
/// Like [`FieldDescriptor`], `EnumDescriptor`, and
/// [`ModelDescriptor`], every text field is a `&'static str` so the
/// descriptor literal emitted by the macro is `inventory::submit!`-
/// submittable. Adding a field is a breaking change for every
/// existing `DerivedProjection` literal site — only do it when load-
/// bearing.
/// # Layout
/// - `name` — the entry's `name = ...` (also the SELECT alias and
///   the visage struct's field name).
/// - `ty_path` — token-string form of the entry's `ty = ...`,
///   captured as the macro emitted it (token-level whitespace
///   preserved; e.g. `"Site"`, `"Option < String >"`,
///   `"crate :: domain :: Site"`). Documentation generators consume
///   this verbatim rather than re-parsing it.
/// - `sql` — the adopter's Postgres SQL expression (verbatim).
/// - `rust` — the adopter's Rust expression source (verbatim).
/// - `doc` — `Some("...")` when the entry declared `doc = "..."`,
///   `None` otherwise.
/// - `scopes` — every scope the entry was declared against, in source
///   order. The per-`(Model, scope)` [`VisageDescriptor`] already
///   keys on scope, but carrying the original set lets consumers
///   walking across visages reconcile multi-scope declarations
///   without re-walking the source model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedProjection {
    /// Output field name (the entry's `name = ...`).
    pub name: &'static str,
    /// Fully-qualified Rust type path for the output field, captured
    /// as a token-string (the entry's `ty = ...`). Kept as
    /// `&'static str` so consumers see the source spelling rather
    /// than a re-parsed form.
    pub ty_path: &'static str,
    /// The adopter's Postgres SQL expression (the entry's
    /// `sql = "..."`). Verbatim — the same string the macro splices
    /// into `PROJECTION_LIST` (with outer parentheses added at SELECT
    /// emission time, not here).
    pub sql: &'static str,
    /// The adopter's Rust expression (the entry's `rust = "..."`).
    /// Surfaced for documentation; not re-parsed by the descriptor
    /// consumer.
    pub rust: &'static str,
    /// Optional rustdoc captured from `doc = "..."`. `None` when the
    /// entry did not declare `doc`.
    pub doc: Option<&'static str>,
    /// Scopes the entry was declared against, in source order
    /// (`&["public", "admin", "export"]` for a multi-scope shared
    /// declaration).
    pub scopes: &'static [&'static str],
}

/// One descriptor per `(Model, scope)` pair that has at least one
/// derived projection entry in scope — #231.
/// Collected via [`inventory::collect!`] keyed on the
/// [`VisageDescriptor`] type. The collection is **structurally
/// separate** from the [`ModelDescriptor`] / [`EnumDescriptor`]
/// collections that the migration differ walks — migration /
/// snapshot / `build.rs` paths never observe `VisageDescriptor`
/// entries, preserving the storage-vs-projection split the visage-
/// derived-fields surface establishes.
/// # Why a per-`(Model, scope)` descriptor instead of one per model
/// Each scope filters the derived entries through its own
/// `scopes = [...]` membership check at macro time. Two scopes on
/// the same model may project different derived-entry sets (an
/// `admin`-only derived alongside a multi-scope shared one). One
/// descriptor per `(Model, scope)` keeps the entry set keyed on the
/// scope without forcing consumers to re-filter.
/// # Why not on `ModelDescriptor`
/// `ModelDescriptor` is the storage-side source of truth that the
/// migration differ, the `target/djogi_models.json` snapshot
/// channel, and `build.rs` all consume. Adding a `derived` field to
/// it would either (a) bleed projection state into the migration
/// inventory or (b) introduce a `#[serde(skip)] derived: ...` slot
/// that leaks a trap for any future descriptor consumer that walks
/// the struct without the `#[serde]` annotation. A separate
/// inventory surface keeps the boundary mechanical and the migration
/// path strictly storage-shaped.
/// # Emission contract
/// The `#[model]` macro emits one `inventory::submit!(VisageDescriptor
/// { ... })` block per `(Model, scope)` pair for which `scope_derived()`
/// returns at least one entry. `pk = None` source models are skipped
/// (they have no `Model::table_name()`, hence no SELECT projection,
/// hence nothing meaningful for a descriptor to describe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisageDescriptor {
    /// Source model type name — e.g. `"Consignment"`.
    pub model_name: &'static str,
    /// Visage scope key — `"public"` / `"self_view"` / `"admin"` /
    /// `"export"`. Matches [`crate::DjogiVisage::SCOPE`] for the
    /// corresponding visage struct.
    pub scope: &'static str,
    /// Visage struct type name — e.g. `"ConsignmentPublic"`. Equals
    /// `"<model_name><PascalCaseScope>"` per the
    /// `djogi-macros::model::visages::SCOPES` mapping table; carried
    /// here so descriptor consumers do not have to reconstruct the
    /// name from the scope key.
    pub visage_name: &'static str,
    /// Derived projection entries in struct-field order — the same
    /// order the macro emits the visage struct fields, the SELECT
    /// alias positions, and the in-memory init expressions. Empty
    /// slices are not emitted (the macro skips
    /// `inventory::submit!` when `scope_derived()` is empty).
    pub derived: &'static [DerivedProjection],
}

inventory::collect!(VisageDescriptor);
