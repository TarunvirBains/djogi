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

/// Returns the Postgres SQL type cast suffix for `V`, or `None` for `String`
/// and `&str` (text extraction already produces text — no cast needed).
///
/// Matching uses `std::any::type_name::<V>()`. Primitive types (`i16`,
/// `i32`, `f32`, `bool`, …) return their short form. Types from external
/// crates return their **fully-qualified path including private module
/// segments** — e.g. `time::offset_date_time::OffsetDateTime` rather
/// than the public re-export `time::OffsetDateTime`. The match arms
/// below carry both the canonical `type_name` output and the public
/// re-export string defensively (test fixtures and hand-written
/// callers may use either form). All known `IntoFilterValue`
/// implementors are explicitly mapped; an unknown type falls through
/// to `None` (compared as text).
///
/// Every `IntoFilterValue` implementor must appear in this table. If a
/// new implementor is added to `query::field` without a corresponding
/// cast arm here, JSONB path comparisons for that type will silently
/// use text comparison on the Postgres side.
pub(crate) fn sql_cast_for_type(type_name: &str) -> Option<&'static str> {
    // Plain-English rule: known numeric / temporal / UUID types gain an
    // explicit Postgres-side cast so comparisons work correctly. Strings
    // need none — text extraction already yields TEXT.
    match type_name {
        // Integer types — Postgres cast names match the SQL standard.
        "i16" => Some("::int2"),
        "i32" => Some("::int4"),
        "i64" => Some("::int8"),
        // Narrow integers (Phase 7-Zero-2 polish, GH issue #29). Each
        // narrow type widens to the smallest signed Postgres type that
        // fits its full range. Mirrors the `IntoFilterValue` impls in
        // `query::field`. `u64` is deliberately absent because its
        // range exceeds `int8`; bind via `numeric` (`rust_decimal::Decimal`).
        "i8" => Some("::int2"),
        "u8" => Some("::int2"),
        "u16" => Some("::int4"),
        "u32" => Some("::int8"),
        // Floating-point types.
        "f32" => Some("::float4"),
        "f64" => Some("::float8"),
        // Boolean.
        "bool" => Some("::boolean"),
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
            Some("::timestamptz")
        }
        "time::date::Date" | "time::Date" | "Date" => Some("::date"),
        // UUID — applies to both uuid::Uuid directly and djogi's RanjId,
        // which is a newtype over uuid::Uuid with the same wire format.
        "uuid::Uuid" | "Uuid" => Some("::uuid"),
        // HeerId — `type_name<heeranjid::HeerId>()` is
        // `heeranjid::heer::HeerId`. The short re-export form
        // `heeranjid::HeerId` and djogi's `djogi::types::HeerId` alias
        // (which `type_name` would never produce — aliases resolve at
        // monomorphisation — but defensive against hand-written
        // strings) are also accepted.
        "heeranjid::heer::HeerId" | "djogi::types::HeerId" | "heeranjid::HeerId" => Some("::int8"),
        // HeerIdDesc — descending-order variant; `IntoFilterValue`
        // exists at `djogi/src/query/field.rs:461`. Real `type_name`
        // is `heeranjid::heer_desc::HeerIdDesc`. Codex round-2 BLOCK
        // (Cluster F finding 1) caught this gap — JSONB comparisons
        // against a `HeerIdDesc`-typed value were silently falling
        // back to text. The `HeerIdRecencyBiased` re-export alias
        // resolves to the same type; one arm covers both.
        "heeranjid::heer_desc::HeerIdDesc"
        | "djogi::types::HeerIdDesc"
        | "heeranjid::HeerIdDesc" => Some("::int8"),
        // RanjId — same shape as HeerId. Real `type_name` is
        // `heeranjid::ranj::RanjId`; aliases preserved for parity.
        "heeranjid::ranj::RanjId" | "djogi::types::RanjId" | "heeranjid::RanjId" => Some("::uuid"),
        // RanjIdDesc — same coverage gap as HeerIdDesc.
        "heeranjid::ranj_desc::RanjIdDesc"
        | "djogi::types::RanjIdDesc"
        | "heeranjid::RanjIdDesc" => Some("::uuid"),
        // rust_decimal::Decimal — stored as NUMERIC in Postgres.
        // Real `type_name` is `rust_decimal::decimal::Decimal`.
        "rust_decimal::decimal::Decimal" | "rust_decimal::Decimal" | "Decimal" => Some("::numeric"),
        // alloc::string::String / &str — text extraction already yields TEXT,
        // no cast needed. Both spellings are listed defensively.
        "alloc::string::String" | "String" | "&str" | "str" => None,
        // Unknown type — fall back to no cast (text comparison). Callers
        // who hit this branch for a type that genuinely needs a cast will
        // observe wrong results; the correct fix is to add a new arm above.
        _ => None,
    }
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
#[derive(Debug, Clone)]
pub struct JsonbPathLeaf {
    /// JSONB column name — a `&'static str` validated by `JsonbPathRef::new`.
    pub column: &'static str,
    /// Dotted path string, e.g. `"engine.cylinders"`. Each segment was
    /// validated by [`validate_dotted_path`] before storage.
    pub path: &'static str,
    /// Optional Postgres cast suffix, e.g. `"::int4"`. `None` for string
    /// and other text-compatible types.
    pub cast: Option<&'static str>,
    /// The comparison operator.
    pub op: crate::query::condition::LookupOp,
    /// The bound value.
    pub value: FilterValue,
}

// ── Comparison surface for JsonbPathRef<M, V> ─────────────────────────────

use crate::query::field::IntoFilterValue;

impl<M: Model, V: IntoFilterValue + 'static> JsonbPathRef<M, V> {
    /// Return the Postgres cast suffix for `V` based on `type_name::<V>()`.
    fn cast_for_v() -> Option<&'static str> {
        sql_cast_for_type(std::any::type_name::<V>())
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
}
