//! Flat JSONB path querying — `FieldRef<M, Jsonb<T>>::path::<V>("dot.path")`.
//!
//! # What
//!
//! [`JsonbPathRef<M, V>`] is produced by calling `.path::<V>("a.b.c")` on a
//! `FieldRef<M, Jsonb<T>>`. It carries the JSONB column name and a dotted path
//! string, and exposes the same comparison surface as `FieldRef<M, V>`:
//! `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in_list`, `is_null`,
//! `is_not_null`.
//!
//! # SQL emission shape
//!
//! Given column `specs` and path `"engine.cylinders"` with type `i32`:
//!
//! ```sql
//! (specs->'engine'->>'cylinders')::int
//! ```
//!
//! The last path segment uses the text-extraction operator `->>'seg'`; every
//! preceding segment uses the object-navigation operator `->'seg'`. The
//! resulting text is then cast to the SQL type corresponding to `V`.
//!
//! # Path identifier validation
//!
//! Each `.`-separated segment must be non-empty, begin with an ASCII letter or
//! underscore, contain only ASCII alphanumerics or underscores, and be at most
//! 63 bytes long. This rule is enforced in plain English — no regex engine is
//! used or allowed anywhere in Djogi (see `decisions.md`).
//!
//! # Why flat only?
//!
//! `path()` is intentionally shallow: it accepts a dotted string (evaluated at
//! runtime) and emits a direct `->` chain. Typed deep paths (a full
//! `#[derive(JsonbSchema)]` with compile-time field access) are deferred to
//! Task 6. The flat API already covers the overwhelmingly common case — most
//! real JSONB filter predicates reference one or two nesting levels.

use crate::model::Model;
use crate::query::condition::{Condition, FilterValue};
use std::marker::PhantomData;

/// Validates a single JSONB path segment.
///
/// Each segment must:
///
/// - Be non-empty.
/// - Begin with an ASCII letter (`a`–`z`, `A`–`Z`) or underscore (`_`).
/// - Contain only ASCII alphanumerics (`a`–`z`, `A`–`Z`, `0`–`9`) or
///   underscores (`_`).
/// - Be at most 63 bytes long (the Postgres `NAMEDATALEN - 1` limit).
///
/// Returns `true` if the segment satisfies every rule, `false` otherwise.
/// No regex engine is used — all checks are byte-level stdlib primitives.
pub(crate) fn is_plain_ident(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Validate a full dotted path string. Each `.`-delimited segment must pass
/// [`is_plain_ident`]. Panics with a descriptive message on violation.
fn validate_dotted_path(dotted: &'static str) {
    if dotted.is_empty() {
        panic!("Djogi: JsonbPathRef path must be non-empty (got empty string)");
    }
    for segment in dotted.split('.') {
        if !is_plain_ident(segment) {
            panic!(
                "Djogi: JsonbPathRef path segment {segment:?} is not a valid plain identifier. \
                 Each segment must be non-empty, begin with an ASCII letter or underscore, \
                 contain only ASCII alphanumerics or underscores, and be at most 63 bytes long."
            );
        }
    }
}

/// A typed handle for filtering on a JSONB sub-path.
///
/// Produced by [`crate::query::field::FieldRef::path`] on a
/// `FieldRef<M, Jsonb<T>>`. `V` is the Rust type the path segment is expected
/// to hold; it determines the Postgres cast applied to the text-extracted
/// value (e.g. `::int` for `i32`, `::bigint` for `i64`).
///
/// `JsonbPathRef` is `Copy` — it holds only two `&'static str` pointers and
/// two phantom markers.
pub struct JsonbPathRef<M, V> {
    column: &'static str,
    path: &'static str,
    _phantom: PhantomData<fn() -> (M, V)>,
}

impl<M, V> Copy for JsonbPathRef<M, V> {}
impl<M, V> Clone for JsonbPathRef<M, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M, V> std::fmt::Debug for JsonbPathRef<M, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsonbPathRef({}.{})", self.column, self.path)
    }
}

impl<M, V> JsonbPathRef<M, V> {
    /// Construct a new `JsonbPathRef`. Validates every segment of the dotted
    /// path via [`validate_dotted_path`] before storing it. Panics if any
    /// segment violates the plain-identifier rules.
    pub(crate) fn new(column: &'static str, path: &'static str) -> Self {
        validate_dotted_path(path);
        JsonbPathRef {
            column,
            path,
            _phantom: PhantomData,
        }
    }

    /// Construct a `JsonbPathRef` from macro-generated code in user crates.
    ///
    /// Exposed for `#[derive(JsonbSchema)]` output. Validates the path.
    ///
    /// # Panics
    ///
    /// Panics if any segment of `path` violates the plain-identifier rules
    /// (same rules as [`validate_dotted_path`]). The `#[derive(JsonbSchema)]`
    /// macro only emits field name literals as segments, so this is a
    /// safety net rather than a normal code path.
    #[doc(hidden)]
    pub fn __from_macro(column: &'static str, path: &'static str) -> Self {
        validate_dotted_path(path);
        JsonbPathRef {
            column,
            path,
            _phantom: PhantomData,
        }
    }

    /// The column name, for SQL emission.
    #[doc(hidden)]
    pub fn column(self) -> &'static str {
        self.column
    }

    /// The dotted path string, for SQL emission.
    #[doc(hidden)]
    pub fn path_str(self) -> &'static str {
        self.path
    }
}

/// Closed taxonomy of Postgres casts a JSONB path LHS can wear.
///
/// `Jsonb<T>` path extraction (`(col->...->>'key')`) always yields TEXT.
/// To compare against a numeric, temporal, UUID, decimal, interval, or
/// network bind value, the LHS must be cast to the matching Postgres type
/// before the comparison runs — otherwise Postgres compares as text and
/// `'10' < '9'` because text ordering is lexicographic, not numeric.
///
/// This enum is the typed public API surface for that cast metadata.
/// Adopter-supplied wrapper types delegate JSONB cast selection through
/// the value-typed [`IntoFilterValue::jsonb_sql_cast`] trait method,
/// which returns a variant of this enum — never a free-form SQL string.
/// That keeps every cast that ever reaches the SQL emitter constrained
/// to the closed set below.
///
/// # Variants
///
/// - [`Int2`](Self::Int2): `::int2` — `i8` / `i16` / `u8` widening.
/// - [`Int4`](Self::Int4): `::int4` — `i32` / `u16` widening.
/// - [`Int8`](Self::Int8): `::int8` — `i64` / `u32` widening / HeerId family.
/// - [`Float4`](Self::Float4): `::float4` — `f32`.
/// - [`Float8`](Self::Float8): `::float8` — `f64`.
/// - [`Boolean`](Self::Boolean): `::boolean`.
/// - [`Timestamptz`](Self::Timestamptz): `::timestamptz` — `time::OffsetDateTime`.
/// - [`Date`](Self::Date): `::date` — `time::Date`.
/// - [`Uuid`](Self::Uuid): `::uuid` — `uuid::Uuid` / RanjId family.
/// - [`Numeric`](Self::Numeric): `::numeric` — `rust_decimal::Decimal`, `u64`.
/// - [`Interval`](Self::Interval): `::interval` — `djogi::Interval`.
/// - `Inet`: `::inet` — `std::net::IpAddr` (`network` feature only).
/// - `Cidr`: `::cidr` — `djogi::CidrAddr` (`network` feature only).
/// - `Macaddr`: `::macaddr` — `djogi::MacAddr` (`network` feature only).
///
/// The enum is `#[non_exhaustive]` so future Postgres-cast surface (e.g.
/// `::bytea`, `::tstzrange`, …) can be added without a SemVer break on
/// downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JsonbSqlCast {
    /// `::int2` (Postgres SMALLINT, 16-bit signed).
    Int2,
    /// `::int4` (Postgres INTEGER, 32-bit signed).
    Int4,
    /// `::int8` (Postgres BIGINT, 64-bit signed).
    Int8,
    /// `::float4` (Postgres REAL, 32-bit IEEE 754).
    Float4,
    /// `::float8` (Postgres DOUBLE PRECISION, 64-bit IEEE 754).
    Float8,
    /// `::boolean`.
    Boolean,
    /// `::timestamptz` (Postgres TIMESTAMP WITH TIME ZONE).
    Timestamptz,
    /// `::date`.
    Date,
    /// `::uuid`.
    Uuid,
    /// `::numeric` (Postgres exact-precision decimal).
    Numeric,
    /// `::interval`.
    Interval,
    /// `::inet` — `network` feature only.
    #[cfg(feature = "network")]
    Inet,
    /// `::cidr` — `network` feature only.
    #[cfg(feature = "network")]
    Cidr,
    /// `::macaddr` — `network` feature only.
    #[cfg(feature = "network")]
    Macaddr,
}

impl JsonbSqlCast {
    /// SQL cast suffix string the emitter splices after the parenthesised
    /// JSONB extraction expression (e.g. `(col->>'key')` + `"::int8"`).
    ///
    /// `pub(crate)` because the only legitimate consumer is the JSONB
    /// path SQL emitter inside this crate. Adopters select the variant
    /// through [`IntoFilterValue::jsonb_sql_cast`]; the string form is
    /// an implementation detail of how the framework renders the LHS.
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            JsonbSqlCast::Int2 => "::int2",
            JsonbSqlCast::Int4 => "::int4",
            JsonbSqlCast::Int8 => "::int8",
            JsonbSqlCast::Float4 => "::float4",
            JsonbSqlCast::Float8 => "::float8",
            JsonbSqlCast::Boolean => "::boolean",
            JsonbSqlCast::Timestamptz => "::timestamptz",
            JsonbSqlCast::Date => "::date",
            JsonbSqlCast::Uuid => "::uuid",
            JsonbSqlCast::Numeric => "::numeric",
            JsonbSqlCast::Interval => "::interval",
            #[cfg(feature = "network")]
            JsonbSqlCast::Inet => "::inet",
            #[cfg(feature = "network")]
            JsonbSqlCast::Cidr => "::cidr",
            #[cfg(feature = "network")]
            JsonbSqlCast::Macaddr => "::macaddr",
        }
    }
}

/// Resolve the typed [`JsonbSqlCast`] for a `std::any::type_name::<V>()`
/// string, or `None` for `String` / `&str` (text extraction already
/// produces text — no cast needed) and for any type not in the table.
///
/// This is the canonical lookup behind [`IntoFilterValue::jsonb_sql_cast`]'s
/// default body. Primitive types (`i16`, `i32`, `f32`, `bool`, …) return
/// their short form. Types from external crates return their
/// **fully-qualified path including private module segments** — e.g.
/// `time::offset_date_time::OffsetDateTime` rather than the public
/// re-export `time::OffsetDateTime`. The match arms below carry both the
/// canonical `type_name` output and the public re-export string
/// defensively (test fixtures and hand-written callers may use either
/// form). All known [`IntoFilterValue`] implementors are explicitly
/// mapped; an unknown type falls through to `None`.
///
/// Every [`IntoFilterValue`] implementor whose Rust value type maps to a
/// non-text Postgres column must appear in this table. If a new
/// implementor is added to `query::field` without a corresponding cast
/// arm here, JSONB path comparisons for that type will silently use text
/// comparison on the Postgres side — `'10' < '9'` because text ordering
/// is lexicographic, not numeric.
pub(crate) fn jsonb_sql_cast_for_type(type_name: &str) -> Option<JsonbSqlCast> {
    // Plain-English rule: known numeric / temporal / UUID types gain an
    // explicit Postgres-side cast so comparisons work correctly. Strings
    // need none — text extraction already yields TEXT.
    match type_name {
        // Integer types — Postgres cast names match the SQL standard.
        "i16" => Some(JsonbSqlCast::Int2),
        "i32" => Some(JsonbSqlCast::Int4),
        "i64" => Some(JsonbSqlCast::Int8),
        // Narrow integers (Phase 7-Zero-2 polish, GH issue #29) plus
        // `u64` (djogi#161 / Phase 8.5 v3 Cluster 2). Each narrow type
        // widens to the smallest signed Postgres type that fits its full
        // range; `u64` exceeds `int8`'s positive range, so it widens to
        // bare `NUMERIC` and binds via `rust_decimal::Decimal` (see
        // `IntoFilterValue for u64` in `query::field`).
        "i8" => Some(JsonbSqlCast::Int2),
        "u8" => Some(JsonbSqlCast::Int2),
        "u16" => Some(JsonbSqlCast::Int4),
        "u32" => Some(JsonbSqlCast::Int8),
        "u64" => Some(JsonbSqlCast::Numeric),
        // Floating-point types.
        "f32" => Some(JsonbSqlCast::Float4),
        "f64" => Some(JsonbSqlCast::Float8),
        // Boolean.
        "bool" => Some(JsonbSqlCast::Boolean),
        // Temporal types — `std::any::type_name::<T>()` returns the FULL
        // path including private modules, so the canonical match strings
        // are `time::offset_date_time::OffsetDateTime` and `time::date::Date`,
        // not the public re-export paths. The short-form arms
        // (`"OffsetDateTime"`, `"Date"`, `"time::OffsetDateTime"`, etc.) are
        // kept defensively in case a caller passes a hand-written name
        // (test fixtures do this) or a future rustc release simplifies
        // the format. Codex round-1 BLOCK (Cluster A finding 1) caught
        // that the table mapped only the public-path forms while
        // `type_name<>()` produced the full forms — every temporal jsonb
        // path comparison was silently falling back to text.
        "time::offset_date_time::OffsetDateTime" | "time::OffsetDateTime" | "OffsetDateTime" => {
            Some(JsonbSqlCast::Timestamptz)
        }
        "time::date::Date" | "time::Date" | "Date" => Some(JsonbSqlCast::Date),
        // UUID — applies to both uuid::Uuid directly and djogi's RanjId,
        // which is a newtype over uuid::Uuid with the same wire format.
        "uuid::Uuid" | "Uuid" => Some(JsonbSqlCast::Uuid),
        // HeerId — `type_name<heeranjid::HeerId>()` is
        // `heeranjid::heer::HeerId`. The short re-export form
        // `heeranjid::HeerId` and djogi's `djogi::types::HeerId` alias
        // (which `type_name` would never produce — aliases resolve at
        // monomorphisation — but defensive against hand-written
        // strings) are also accepted.
        "heeranjid::heer::HeerId" | "djogi::types::HeerId" | "heeranjid::HeerId" => {
            Some(JsonbSqlCast::Int8)
        }
        // HeerIdDesc — descending-order variant; `IntoFilterValue`
        // exists at `djogi/src/query/field.rs:461`. Real `type_name`
        // is `heeranjid::heer_desc::HeerIdDesc`. Codex round-2 BLOCK
        // (Cluster F finding 1) caught this gap — JSONB comparisons
        // against a `HeerIdDesc`-typed value were silently falling
        // back to text. The `HeerIdRecencyBiased` re-export alias
        // resolves to the same type; one arm covers both.
        "heeranjid::heer_desc::HeerIdDesc"
        | "djogi::types::HeerIdDesc"
        | "heeranjid::HeerIdDesc" => Some(JsonbSqlCast::Int8),
        // RanjId — same shape as HeerId. Real `type_name` is
        // `heeranjid::ranj::RanjId`; aliases preserved for parity.
        "heeranjid::ranj::RanjId" | "djogi::types::RanjId" | "heeranjid::RanjId" => {
            Some(JsonbSqlCast::Uuid)
        }
        // RanjIdDesc — same coverage gap as HeerIdDesc.
        "heeranjid::ranj_desc::RanjIdDesc"
        | "djogi::types::RanjIdDesc"
        | "heeranjid::RanjIdDesc" => Some(JsonbSqlCast::Uuid),
        // rust_decimal::Decimal — stored as NUMERIC in Postgres.
        // Real `type_name` is `rust_decimal::decimal::Decimal`.
        "rust_decimal::decimal::Decimal" | "rust_decimal::Decimal" | "Decimal" => {
            Some(JsonbSqlCast::Numeric)
        }
        // Interval (djogi#212) — djogi's own newtype wrapping the Postgres
        // `INTERVAL` wire format. The struct is defined in `djogi::pg_types`
        // (not a private sub-module), so `type_name::<djogi::Interval>()`
        // produces `djogi::pg_types::Interval` at runtime. The public
        // re-export alias `djogi::types::Interval` and the bare name
        // `Interval` are kept defensively; re-export aliases are never
        // returned by `type_name`, but hand-written test strings may use
        // them. Inside a JSONB column an interval is stored as its ISO 8601
        // text representation (e.g. `"P1M2DT3.5S"`); the `->>'key'`
        // text-extraction operator produces that text, which Postgres can
        // then cast to `interval` for a correct typed comparison.
        "djogi::pg_types::Interval" | "djogi::types::Interval" | "Interval" => {
            Some(JsonbSqlCast::Interval)
        }
        // Network family (djogi#213) — INET / CIDR / MACADDR text
        // extraction inside JSONB columns produces the canonical
        // Postgres text form (`192.168.1.0`, `10.0.0.0/8`,
        // `aa:bb:cc:dd:ee:ff`). Casting to the typed column type lets
        // Postgres normalise the value (e.g., trim leading zeros in
        // IPv4 octets) before comparison.
        //
        // `IpAddr` is `std::net::IpAddr`; `type_name` returns
        // `core::net::ip_addr::IpAddr` at runtime under stable Rust.
        // `MacAddr` / `CidrAddr` live in `djogi::pg_types` (not a
        // private submodule), so `type_name` produces
        // `djogi::pg_types::MacAddr` / `djogi::pg_types::CidrAddr`.
        // Defensive aliases (`djogi::types::*`, bare names) included
        // for hand-written test strings as elsewhere in this match.
        //
        // Match arms stay live (not feature-gated) so the string lookup
        // works the same with or without the `network` feature, but the
        // returned variants only resolve when the feature is on. Without
        // the feature the arm body fails to type-check, so the entire
        // family is gated.
        #[cfg(feature = "network")]
        "core::net::ip_addr::IpAddr" | "std::net::IpAddr" | "core::net::IpAddr" | "IpAddr" => {
            Some(JsonbSqlCast::Inet)
        }
        #[cfg(feature = "network")]
        "djogi::pg_types::CidrAddr" | "djogi::types::CidrAddr" | "CidrAddr" => {
            Some(JsonbSqlCast::Cidr)
        }
        #[cfg(feature = "network")]
        "djogi::pg_types::MacAddr" | "djogi::types::MacAddr" | "MacAddr" => {
            Some(JsonbSqlCast::Macaddr)
        }
        // alloc::string::String / &str — text extraction already yields TEXT,
        // no cast needed. Both spellings are listed defensively.
        "alloc::string::String" | "String" | "&str" | "str" => None,
        // Unknown type — fall back to no cast (text comparison). Callers
        // who hit this branch for a type that genuinely needs a cast will
        // observe wrong results; the correct fix is to add a new arm above
        // or implement [`IntoFilterValue::jsonb_sql_cast`] on the wrapper.
        _ => None,
    }
}

/// Returns the Postgres SQL type cast suffix for `V`, or `None` for `String`
/// and `&str` (text extraction already produces text — no cast needed).
///
/// Compatibility shim that wraps [`jsonb_sql_cast_for_type`] —
/// preserves the pre-djogi#161 string-returning API for the in-crate
/// regression tests below. Live SQL emission now reaches the cast
/// metadata through [`IntoFilterValue::jsonb_sql_cast`] →
/// [`JsonbSqlCast::suffix`] so adopter wrappers can delegate cast
/// selection to their inner SQL value type.
#[cfg(test)]
pub(crate) fn sql_cast_for_type(type_name: &str) -> Option<&'static str> {
    jsonb_sql_cast_for_type(type_name).map(JsonbSqlCast::suffix)
}

/// Build the SQL fragment for a JSONB path extraction with optional cast.
///
/// Given `column = "specs"`, `path = "engine.cylinders"`, and
/// `cast = Some("::int")` this produces:
///
/// ```text
/// (specs->'engine'->>'cylinders')::int
/// ```
///
/// The last segment always uses `->>'seg'` (text extraction); every prior
/// segment uses `->'seg'` (object navigation). The entire expression is
/// parenthesised before the cast is appended.
///
/// Production SQL emission now happens inside `emit_jsonb_path_leaf` in
/// `query::sql` (which also handles `parent_table` qualification). This
/// function is kept for unit-test assertions on the SQL shape.
#[cfg(test)]
pub(crate) fn build_path_sql(
    column: &'static str,
    path: &'static str,
    cast: Option<&'static str>,
) -> String {
    let segments: Vec<&str> = path.split('.').collect();
    let mut s = String::new();
    s.push('(');
    s.push_str(column);
    for (i, seg) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            // Last segment: text extraction.
            s.push_str("->>'");
            s.push_str(seg);
            s.push('\'');
        } else {
            // Intermediate segment: object navigation.
            s.push_str("->'");
            s.push_str(seg);
            s.push('\'');
        }
    }
    s.push(')');
    if let Some(c) = cast {
        s.push_str(c);
    }
    s
}

/// Newtype condition variant for JSONB path comparisons. Stores the
/// structural components of the path expression so the SQL emitter can
/// qualify the column reference with a parent table name when rendering
/// inside a `SELECT ... LEFT JOIN` context.
///
/// Stored in [`Condition::JsonbPath`].
///
/// # Why structured, not pre-rendered
///
/// If the expression SQL (`(col->'a'->>'b')::int`) were pre-rendered at
/// `JsonbPathRef` construction time (inside the filter closure), the
/// column name would be bare — `col` not `parent.col`. Inside a joined
/// query (`build_select_joined`) Postgres raises `42702 column reference
/// "col" is ambiguous` when a bare column name also appears on the
/// joined child. Storing the parts and rendering in the emitter lets
/// `emit_jsonb_path_leaf` qualify the column consistently with every
/// other `emit_leaf` arm.
/// Fields are `pub(crate)` so the only construction path is through the
/// typed [`JsonbPathRef`] methods (`eq`, `neq`, `gt`, `gte`, `lt`, `lte`,
/// `is_null`, `is_not_null`, `in_list`). The emitter assumes the typed
/// surface for `op` (no `Regex`/`IRegex`/`Between` variants), so widening
/// the fields would let downstream code construct ill-formed leaves that
/// the emitter renders incorrectly or panics on.
#[derive(Debug, Clone)]
pub struct JsonbPathLeaf {
    /// JSONB column name — a `&'static str` validated by `JsonbPathRef::new`.
    pub(crate) column: &'static str,
    /// Dotted path string, e.g. `"engine.cylinders"`. Each segment was
    /// validated by [`validate_dotted_path`] before storage.
    pub(crate) path: &'static str,
    /// Optional Postgres cast suffix, e.g. `"::int4"`. `None` for string
    /// and other text-compatible types.
    pub(crate) cast: Option<&'static str>,
    /// The comparison operator.
    pub(crate) op: crate::query::condition::LookupOp,
    /// The bound value.
    pub(crate) value: FilterValue,
}

// ── Comparison surface for JsonbPathRef<M, V> ─────────────────────────────

use crate::query::field::IntoFilterValue;

impl<M: Model, V: IntoFilterValue + 'static> JsonbPathRef<M, V> {
    /// Return the Postgres cast suffix for `V`.
    ///
    /// Routes through the typed [`IntoFilterValue::jsonb_sql_cast`]
    /// dispatch (djogi#161) — wrapper types like `primary_key!`-emitted
    /// custom PKs, `ForeignKey<T>`, and `OneToOneField<T>` override the
    /// default body to delegate to their inner SQL value type, so JSONB
    /// path comparisons against those wrappers emit the same typed cast
    /// they would emit against the underlying scalar. The fallback path
    /// (the default impl on `IntoFilterValue`) still walks the
    /// `type_name`-based lookup table for built-in primitives.
    fn cast_for_v() -> Option<&'static str> {
        V::jsonb_sql_cast().map(JsonbSqlCast::suffix)
    }

    fn leaf_condition(
        self,
        op: crate::query::condition::LookupOp,
        value: FilterValue,
    ) -> Condition {
        Condition::JsonbPath(JsonbPathLeaf {
            column: self.column,
            path: self.path,
            cast: Self::cast_for_v(),
            op,
            value,
        })
    }

    /// `(col->...'key')::cast = value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn eq(self, value: V) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::Eq, value.into_filter_value())
    }

    /// `(col->...'key')::cast <> value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn neq(self, value: V) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::Neq, value.into_filter_value())
    }

    /// `(col->...'key')::cast > value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gt(self, value: V) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::Gt, value.into_filter_value())
    }

    /// `(col->...'key')::cast >= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn gte(self, value: V) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::Gte, value.into_filter_value())
    }

    /// `(col->...'key')::cast < value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lt(self, value: V) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::Lt, value.into_filter_value())
    }

    /// `(col->...'key')::cast <= value`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn lte(self, value: V) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::Lte, value.into_filter_value())
    }

    /// `(col->...'key')::cast IS NULL`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_null(self) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::IsNull, FilterValue::Null)
    }

    /// `(col->...'key')::cast IS NOT NULL`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn is_not_null(self) -> Condition {
        use crate::query::condition::LookupOp;
        self.leaf_condition(LookupOp::IsNotNull, FilterValue::Null)
    }

    /// `(col->...'key')::cast IN (v1, v2, …)`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn in_list<I: IntoIterator<Item = V>>(self, values: I) -> Condition {
        use crate::query::condition::LookupOp;
        let list = FilterValue::List(
            values
                .into_iter()
                .map(IntoFilterValue::into_filter_value)
                .collect(),
        );
        self.leaf_condition(LookupOp::In, list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_plain_ident boundary tests ────────────────────────────────────

    #[test]
    fn plain_ident_accepts_simple_name() {
        assert!(is_plain_ident("engine"));
    }

    #[test]
    fn plain_ident_accepts_leading_underscore() {
        assert!(is_plain_ident("_private"));
    }

    #[test]
    fn plain_ident_accepts_internal_underscore() {
        assert!(is_plain_ident("my_field"));
    }

    #[test]
    fn plain_ident_accepts_mixed_alphanumeric() {
        assert!(is_plain_ident("field123"));
    }

    #[test]
    fn plain_ident_rejects_empty() {
        assert!(!is_plain_ident(""));
    }

    #[test]
    fn plain_ident_rejects_starts_with_digit() {
        assert!(!is_plain_ident("1abc"));
    }

    #[test]
    fn plain_ident_rejects_non_ascii() {
        // Multi-byte UTF-8 first byte is never ASCII-alphabetic.
        assert!(!is_plain_ident("café"));
    }

    #[test]
    fn plain_ident_rejects_too_long() {
        // 64 'a' characters — one over the 63-byte limit.
        let s = "a".repeat(64);
        assert!(!is_plain_ident(&s));
    }

    #[test]
    fn plain_ident_accepts_exactly_63_bytes() {
        let s = "a".repeat(63);
        assert!(is_plain_ident(&s));
    }

    #[test]
    fn plain_ident_rejects_hyphen() {
        assert!(!is_plain_ident("my-field"));
    }

    // ── build_path_sql snapshot tests ────────────────────────────────────

    #[test]
    fn build_path_single_segment_with_cast() {
        let sql = build_path_sql("col", "key", Some("::int"));
        assert_eq!(sql, "(col->>'key')::int");
    }

    #[test]
    fn build_path_two_segments_with_cast() {
        let sql = build_path_sql("specs", "engine.cylinders", Some("::int"));
        assert_eq!(sql, "(specs->'engine'->>'cylinders')::int");
    }

    #[test]
    fn build_path_three_segments_no_cast() {
        let sql = build_path_sql("data", "a.b.c", None);
        assert_eq!(sql, "(data->'a'->'b'->>'c')");
    }

    // ── sql_cast_for_type — full matrix coverage ─────────────────────────
    //
    // Every IntoFilterValue implementor must appear in this table with the
    // correct Postgres cast. These tests prove the table is complete and the
    // cast strings are correct. type_name::<V>() returns the form used by the
    // real call site; tests use the same string forms to be faithful.

    #[test]
    fn sql_cast_for_i16() {
        assert_eq!(sql_cast_for_type("i16"), Some("::int2"));
    }

    #[test]
    fn sql_cast_for_i32() {
        assert_eq!(sql_cast_for_type("i32"), Some("::int4"));
    }

    #[test]
    fn sql_cast_for_i64() {
        assert_eq!(sql_cast_for_type("i64"), Some("::int8"));
    }

    // Narrow-integer JSONB casts (Phase 7-Zero-2 polish, GH issue #29).
    // Each maps to the smallest signed Postgres type that fits the
    // narrow Rust type's full range. Mirrors the IntoFilterValue
    // widening in `query::field`.
    #[test]
    fn sql_cast_for_i8() {
        // i8 fits in int2 directly.
        assert_eq!(sql_cast_for_type("i8"), Some("::int2"));
    }

    #[test]
    fn sql_cast_for_u8() {
        // u8 max 255 fits in int2's 32_767 budget.
        assert_eq!(sql_cast_for_type("u8"), Some("::int2"));
    }

    #[test]
    fn sql_cast_for_u16() {
        // u16 max 65_535 exceeds i16 max 32_767, so widen to int4.
        assert_eq!(sql_cast_for_type("u16"), Some("::int4"));
    }

    #[test]
    fn sql_cast_for_u32() {
        // u32 max ~4.3B exceeds i32 max ~2.1B, so widen to int8.
        assert_eq!(sql_cast_for_type("u32"), Some("::int8"));
    }

    #[test]
    fn sql_cast_for_f32() {
        assert_eq!(sql_cast_for_type("f32"), Some("::float4"));
    }

    #[test]
    fn sql_cast_for_f64() {
        assert_eq!(sql_cast_for_type("f64"), Some("::float8"));
    }

    #[test]
    fn sql_cast_for_bool() {
        assert_eq!(sql_cast_for_type("bool"), Some("::boolean"));
    }

    #[test]
    fn sql_cast_for_offset_datetime() {
        assert_eq!(
            sql_cast_for_type("time::OffsetDateTime"),
            Some("::timestamptz")
        );
        assert_eq!(sql_cast_for_type("OffsetDateTime"), Some("::timestamptz"));
    }

    #[test]
    fn sql_cast_for_date() {
        assert_eq!(sql_cast_for_type("time::Date"), Some("::date"));
        assert_eq!(sql_cast_for_type("Date"), Some("::date"));
    }

    #[test]
    fn sql_cast_for_uuid() {
        assert_eq!(sql_cast_for_type("uuid::Uuid"), Some("::uuid"));
        assert_eq!(sql_cast_for_type("Uuid"), Some("::uuid"));
    }

    #[test]
    fn sql_cast_for_heer_id() {
        assert_eq!(sql_cast_for_type("djogi::types::HeerId"), Some("::int8"));
        assert_eq!(sql_cast_for_type("heeranjid::HeerId"), Some("::int8"));
    }

    #[test]
    fn sql_cast_for_ranj_id() {
        assert_eq!(sql_cast_for_type("djogi::types::RanjId"), Some("::uuid"));
        assert_eq!(sql_cast_for_type("heeranjid::RanjId"), Some("::uuid"));
    }

    #[test]
    fn sql_cast_for_decimal() {
        assert_eq!(
            sql_cast_for_type("rust_decimal::Decimal"),
            Some("::numeric")
        );
        assert_eq!(sql_cast_for_type("Decimal"), Some("::numeric"));
    }

    #[test]
    fn sql_cast_for_string_is_none() {
        // alloc::string::String is what type_name::<String>() returns in Rust.
        assert_eq!(sql_cast_for_type("alloc::string::String"), None);
        // Bare "String" also covered for robustness.
        assert_eq!(sql_cast_for_type("String"), None);
    }

    #[test]
    fn sql_cast_for_str_ref_is_none() {
        assert_eq!(sql_cast_for_type("&str"), None);
        assert_eq!(sql_cast_for_type("str"), None);
    }

    #[test]
    fn sql_cast_for_unknown_is_none() {
        assert_eq!(sql_cast_for_type("some::unknown::Type"), None);
    }

    // Codex round-1 BLOCK (Cluster A finding 1) — assert the cast arms
    // against `std::any::type_name::<V>()` output directly, not against
    // hand-written strings. The hand-written `"time::OffsetDateTime"`
    // form is what we *thought* `type_name` produced; the real output
    // is `time::offset_date_time::OffsetDateTime` (the full private-
    // module path). These tests lock the fix in so a future rustc
    // change to the `type_name` format surfaces here, not as silent
    // text-fallback in production JSONB queries.

    #[test]
    fn sql_cast_uses_actual_type_name_for_offset_datetime() {
        let name = std::any::type_name::<::time::OffsetDateTime>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::timestamptz"),
            "type_name<OffsetDateTime>() = {name:?} did not map to ::timestamptz"
        );
    }

    #[test]
    fn sql_cast_uses_actual_type_name_for_date() {
        let name = std::any::type_name::<::time::Date>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::date"),
            "type_name<Date>() = {name:?} did not map to ::date"
        );
    }

    #[test]
    fn sql_cast_uses_actual_type_name_for_uuid() {
        let name = std::any::type_name::<::uuid::Uuid>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::uuid"),
            "type_name<Uuid>() = {name:?} did not map to ::uuid"
        );
    }

    #[test]
    fn sql_cast_uses_actual_type_name_for_heer_id() {
        let name = std::any::type_name::<::heeranjid::HeerId>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::int8"),
            "type_name<HeerId>() = {name:?} did not map to ::int8"
        );
    }

    #[test]
    fn sql_cast_uses_actual_type_name_for_decimal() {
        let name = std::any::type_name::<::rust_decimal::Decimal>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::numeric"),
            "type_name<Decimal>() = {name:?} did not map to ::numeric"
        );
    }

    #[test]
    fn sql_cast_uses_actual_type_name_for_string_returns_none() {
        let name = std::any::type_name::<String>();
        assert_eq!(
            sql_cast_for_type(name),
            None,
            "type_name<String>() = {name:?} should require no cast"
        );
    }

    // Interval cast — hand-written string coverage. All defensive spellings
    // must map to `::interval` so JSONB path comparisons cast the text-
    // extracted LHS before comparing against an INTERVAL bind parameter.
    #[test]
    fn sql_cast_for_interval() {
        // Canonical type_name output — the defining module path.
        assert_eq!(
            sql_cast_for_type("djogi::pg_types::Interval"),
            Some("::interval")
        );
        // Public re-export spellings (defensive — never produced by
        // type_name, but exercised by hand-written test strings).
        assert_eq!(
            sql_cast_for_type("djogi::types::Interval"),
            Some("::interval")
        );
        assert_eq!(sql_cast_for_type("Interval"), Some("::interval"));
    }

    // Lock the actual `type_name::<crate::Interval>()` output against the
    // cast table so any future rustc `type_name` format change surfaces as
    // a test failure here rather than as silent text-fallback in production
    // JSONB interval queries (same pattern as the Codex round-1 temporal
    // fix and the round-2 HeerIdDesc/RanjIdDesc fix).
    #[test]
    fn sql_cast_uses_actual_type_name_for_interval() {
        let name = std::any::type_name::<crate::Interval>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::interval"),
            "type_name<Interval>() = {name:?} did not map to ::interval"
        );
    }

    // djogi#213 — network family JSONB path casts. Same pattern as the
    // Interval casts above: defensive spellings + the `type_name`
    // output anchor so a future rustc format change surfaces here
    // rather than as silent text-fallback in JSONB INET/CIDR/MACADDR
    // path comparisons. Feature-gated per djogi#161 cast-dispatch
    // refactor: `JsonbSqlCast::Inet` is `#[cfg(feature = "network")]`
    // because `IntoFilterValue for std::net::IpAddr` is feature-gated,
    // so the cast variant only resolves through dispatch under the
    // same gate.
    #[cfg(feature = "network")]
    #[test]
    fn sql_cast_for_inet_aliases() {
        // type_name produces `core::net::ip_addr::IpAddr` on stable Rust.
        assert_eq!(
            sql_cast_for_type("core::net::ip_addr::IpAddr"),
            Some("::inet")
        );
        // Hand-written aliases — never produced by type_name but
        // exercised by test strings.
        assert_eq!(sql_cast_for_type("std::net::IpAddr"), Some("::inet"));
        assert_eq!(sql_cast_for_type("core::net::IpAddr"), Some("::inet"));
        assert_eq!(sql_cast_for_type("IpAddr"), Some("::inet"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn sql_cast_uses_actual_type_name_for_ip_addr() {
        let name = std::any::type_name::<std::net::IpAddr>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::inet"),
            "type_name<IpAddr>() = {name:?} did not map to ::inet"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn sql_cast_for_cidr_addr_aliases() {
        assert_eq!(
            sql_cast_for_type("djogi::pg_types::CidrAddr"),
            Some("::cidr")
        );
        assert_eq!(sql_cast_for_type("djogi::types::CidrAddr"), Some("::cidr"));
        assert_eq!(sql_cast_for_type("CidrAddr"), Some("::cidr"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn sql_cast_uses_actual_type_name_for_cidr_addr() {
        let name = std::any::type_name::<crate::CidrAddr>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::cidr"),
            "type_name<CidrAddr>() = {name:?} did not map to ::cidr"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn sql_cast_for_mac_addr_aliases() {
        assert_eq!(
            sql_cast_for_type("djogi::pg_types::MacAddr"),
            Some("::macaddr")
        );
        assert_eq!(
            sql_cast_for_type("djogi::types::MacAddr"),
            Some("::macaddr")
        );
        assert_eq!(sql_cast_for_type("MacAddr"), Some("::macaddr"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn sql_cast_uses_actual_type_name_for_mac_addr() {
        let name = std::any::type_name::<crate::MacAddr>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::macaddr"),
            "type_name<MacAddr>() = {name:?} did not map to ::macaddr"
        );
    }

    // Codex round-2 BLOCK (Cluster F finding 1) — `HeerIdDesc` / `RanjIdDesc`
    // (the descending-order variants of the PK types) implement
    // `IntoFilterValue` and can be used as `JsonbPathRef<M, V>` value
    // generics. They were missing from the cast table — every JSONB
    // comparison against a `HeerIdDesc`-typed payload was silently
    // falling back to text comparison. Lock them in here against the
    // real `type_name<>()` output so a future change surfaces here
    // first.
    #[test]
    fn sql_cast_uses_actual_type_name_for_heer_id_desc() {
        let name = std::any::type_name::<crate::HeerIdDesc>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::int8"),
            "type_name<HeerIdDesc>() = {name:?} did not map to ::int8"
        );
    }

    #[test]
    fn sql_cast_uses_actual_type_name_for_ranj_id_desc() {
        let name = std::any::type_name::<crate::RanjIdDesc>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::uuid"),
            "type_name<RanjIdDesc>() = {name:?} did not map to ::uuid"
        );
    }

    // ── djogi#161 — `u64` JSONB cast (Numeric) ────────────────────────────
    //
    // `u64` exceeds `int8`'s positive range, so it widens through
    // `IntoFilterValue` to `FilterValue::Decimal` (bare NUMERIC bind).
    // The matching JSONB path LHS cast is `::numeric`. Pre-#161 `u64`
    // was deliberately absent from the cast table — JSONB comparisons
    // against a `u64`-typed payload silently fell back to text.

    #[test]
    fn sql_cast_for_u64_numeric() {
        assert_eq!(sql_cast_for_type("u64"), Some("::numeric"));
    }

    #[test]
    fn jsonb_sql_cast_for_u64_numeric() {
        assert_eq!(
            jsonb_sql_cast_for_type("u64"),
            Some(JsonbSqlCast::Numeric),
            "u64 must map to JsonbSqlCast::Numeric"
        );
    }

    #[test]
    fn jsonb_sql_cast_uses_actual_type_name_for_u64() {
        let name = std::any::type_name::<u64>();
        assert_eq!(
            sql_cast_for_type(name),
            Some("::numeric"),
            "type_name<u64>() = {name:?} did not map to ::numeric"
        );
        assert_eq!(
            jsonb_sql_cast_for_type(name),
            Some(JsonbSqlCast::Numeric),
            "type_name<u64>() = {name:?} did not map to JsonbSqlCast::Numeric"
        );
    }

    // ── JsonbSqlCast enum-level assertions ────────────────────────────────
    //
    // The string-returning `sql_cast_for_type` shim is implemented as a
    // wrapper over the typed `jsonb_sql_cast_for_type`. These tests pin
    // the variant ↔ suffix relationship so a typo in `JsonbSqlCast::suffix`
    // surfaces here, not as a silent text-fallback on the SQL emitter
    // side.

    #[test]
    fn jsonb_sql_cast_suffix_round_trips_integer_variants() {
        assert_eq!(JsonbSqlCast::Int2.suffix(), "::int2");
        assert_eq!(JsonbSqlCast::Int4.suffix(), "::int4");
        assert_eq!(JsonbSqlCast::Int8.suffix(), "::int8");
    }

    #[test]
    fn jsonb_sql_cast_suffix_round_trips_float_variants() {
        assert_eq!(JsonbSqlCast::Float4.suffix(), "::float4");
        assert_eq!(JsonbSqlCast::Float8.suffix(), "::float8");
    }

    #[test]
    fn jsonb_sql_cast_suffix_round_trips_misc_variants() {
        assert_eq!(JsonbSqlCast::Boolean.suffix(), "::boolean");
        assert_eq!(JsonbSqlCast::Timestamptz.suffix(), "::timestamptz");
        assert_eq!(JsonbSqlCast::Date.suffix(), "::date");
        assert_eq!(JsonbSqlCast::Uuid.suffix(), "::uuid");
        assert_eq!(JsonbSqlCast::Numeric.suffix(), "::numeric");
        assert_eq!(JsonbSqlCast::Interval.suffix(), "::interval");
    }

    #[test]
    fn jsonb_sql_cast_for_known_built_ins() {
        assert_eq!(jsonb_sql_cast_for_type("i16"), Some(JsonbSqlCast::Int2));
        assert_eq!(jsonb_sql_cast_for_type("i32"), Some(JsonbSqlCast::Int4));
        assert_eq!(jsonb_sql_cast_for_type("i64"), Some(JsonbSqlCast::Int8));
        assert_eq!(jsonb_sql_cast_for_type("f32"), Some(JsonbSqlCast::Float4));
        assert_eq!(jsonb_sql_cast_for_type("f64"), Some(JsonbSqlCast::Float8));
        assert_eq!(jsonb_sql_cast_for_type("bool"), Some(JsonbSqlCast::Boolean));
    }

    #[test]
    fn jsonb_sql_cast_for_string_is_none() {
        assert_eq!(jsonb_sql_cast_for_type("String"), None);
        assert_eq!(jsonb_sql_cast_for_type("alloc::string::String"), None);
        assert_eq!(jsonb_sql_cast_for_type("&str"), None);
        assert_eq!(jsonb_sql_cast_for_type("str"), None);
    }

    #[test]
    fn jsonb_sql_cast_for_unknown_is_none() {
        assert_eq!(jsonb_sql_cast_for_type("some::unknown::Type"), None);
    }

    #[cfg(feature = "network")]
    #[test]
    fn jsonb_sql_cast_suffix_round_trips_network_variants() {
        assert_eq!(JsonbSqlCast::Inet.suffix(), "::inet");
        assert_eq!(JsonbSqlCast::Cidr.suffix(), "::cidr");
        assert_eq!(JsonbSqlCast::Macaddr.suffix(), "::macaddr");
    }

    // ── djogi#161 — `IntoFilterValue::jsonb_sql_cast()` trait dispatch ────
    //
    // The trait method is the canonical adopter-facing entry point. The
    // default impl on `IntoFilterValue` walks `type_name::<Self>()`
    // through `jsonb_sql_cast_for_type`. Wrapper newtypes override the
    // method to delegate to the inner SQL value type. These tests cover
    // the dispatch for built-in primitives and a wrapper that delegates
    // through `i64`.

    use crate::query::field::IntoFilterValue;

    #[test]
    fn into_filter_value_jsonb_sql_cast_resolves_built_in_primitives() {
        assert_eq!(
            <i32 as IntoFilterValue>::jsonb_sql_cast(),
            Some(JsonbSqlCast::Int4)
        );
        assert_eq!(
            <i64 as IntoFilterValue>::jsonb_sql_cast(),
            Some(JsonbSqlCast::Int8)
        );
        assert_eq!(
            <u64 as IntoFilterValue>::jsonb_sql_cast(),
            Some(JsonbSqlCast::Numeric)
        );
        assert_eq!(
            <bool as IntoFilterValue>::jsonb_sql_cast(),
            Some(JsonbSqlCast::Boolean)
        );
        assert_eq!(<String as IntoFilterValue>::jsonb_sql_cast(), None);
    }

    /// Local newtype wrapper. Delegates `IntoFilterValue` and
    /// `jsonb_sql_cast` to `i64`. Mirrors the shape `primary_key!`
    /// emits: the inner type is the SQL value type. Pre-#161 the
    /// wrapper inherited the default (`type_name::<LocalI64Id>()`
    /// returns `djogi::jsonb::path::tests::LocalI64Id` which is not
    /// in the cast table) and silently fell back to text — the
    /// regression this test pins.
    #[derive(Debug, Clone, Copy)]
    struct LocalI64Id(i64);

    impl IntoFilterValue for LocalI64Id {
        fn into_filter_value(self) -> crate::query::condition::FilterValue {
            <i64 as IntoFilterValue>::into_filter_value(self.0)
        }

        fn jsonb_sql_cast() -> Option<JsonbSqlCast> {
            <i64 as IntoFilterValue>::jsonb_sql_cast()
        }
    }

    #[test]
    fn local_newtype_wrapper_delegates_jsonb_sql_cast_to_inner() {
        assert_eq!(
            <LocalI64Id as IntoFilterValue>::jsonb_sql_cast(),
            Some(JsonbSqlCast::Int8),
            "wrapper must delegate cast metadata to its inner SQL value type"
        );
    }

    // Minimal `Model` stub so we can construct a `JsonbPathRef<M, V>`
    // and inspect the cast on the resulting leaf without pulling in
    // `#[derive(Model)]` or a real model registration.
    struct StubModel;
    impl crate::model::__sealed::Sealed for StubModel {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for StubModel {
        type Pk = crate::HeerId;
        type Fields = ();
        fn table_name() -> &'static str {
            "stub"
        }
        fn pk_value(&self) -> &crate::HeerId {
            unreachable!()
        }
        fn descriptor() -> &'static crate::descriptor::ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: crate::HeerId,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    /// djogi#161 SQL-shape pin — when `V` is a wrapper that delegates
    /// `IntoFilterValue::jsonb_sql_cast` to `i64`, the leaf produced
    /// by `JsonbPathRef::<M, LocalI64Id>` must carry `::int8` cast.
    /// The full rendered LHS is `(specs->>'rank')::int8`, asserted via
    /// the test-only `build_path_sql` shape helper.
    #[test]
    fn jsonb_path_ref_builds_int8_cast_for_local_wrapper() {
        use crate::query::condition::Condition;
        let path: JsonbPathRef<StubModel, LocalI64Id> = JsonbPathRef::new("specs", "rank");
        let cond = path.gt(LocalI64Id(9));
        let leaf = match cond {
            Condition::JsonbPath(l) => l,
            other => panic!("expected JsonbPath leaf, got {other:?}"),
        };
        assert_eq!(
            leaf.cast,
            Some("::int8"),
            "JsonbPathRef<_, LocalI64Id> must carry ::int8 cast via delegation"
        );
        // Render the LHS SQL shape with the same cast suffix the emitter
        // splices in for production queries — pins the end-to-end shape
        // pre/post djogi#161.
        let lhs = build_path_sql(leaf.column, leaf.path, leaf.cast);
        assert_eq!(lhs, "(specs->>'rank')::int8");
    }

    /// djogi#161 SQL-shape pin — `JsonbPathRef<_, u64>` must carry
    /// `::numeric` cast (NOT no cast), so `u64` JSONB path comparisons
    /// use Postgres NUMERIC ordering instead of text ordering.
    #[test]
    fn jsonb_path_ref_builds_numeric_cast_for_u64() {
        use crate::query::condition::Condition;
        let path: JsonbPathRef<StubModel, u64> = JsonbPathRef::new("meta", "view_count");
        let cond = path.gt(9u64);
        let leaf = match cond {
            Condition::JsonbPath(l) => l,
            other => panic!("expected JsonbPath leaf, got {other:?}"),
        };
        assert_eq!(
            leaf.cast,
            Some("::numeric"),
            "u64 JSONB path comparisons must cast to ::numeric"
        );
        let lhs = build_path_sql(leaf.column, leaf.path, leaf.cast);
        assert_eq!(lhs, "(meta->>'view_count')::numeric");
    }
}
