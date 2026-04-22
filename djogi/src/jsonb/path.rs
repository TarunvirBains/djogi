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
/// (text extraction already produces text — no cast needed).
///
/// The map is a sorted const slice; matching is exact case-insensitive
/// lookup via `type_name::<V>()`. Only the types Djogi ships with are
/// covered; unknown types default to no cast, which means the value is
/// compared as text.
pub(crate) fn sql_cast_for_type(type_name: &str) -> Option<&'static str> {
    // Plain-English rule: known numeric / temporal types gain an explicit
    // Postgres-side cast so comparisons work correctly. Strings need none.
    match type_name {
        "i16" | "i32" => Some("::int"),
        "i64" => Some("::bigint"),
        "f32" => Some("::float4"),
        "f64" => Some("::float8"),
        "bool" => Some("::bool"),
        "time::OffsetDateTime" | "OffsetDateTime" => Some("::timestamptz"),
        _ => None, // String / &str / unknown — leave as text
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
/// pre-built SQL expression fragment (which is already trusted — built only
/// from validated identifiers) and the comparison operator + bound value.
///
/// Stored in [`Condition::JsonbPath`].
#[derive(Debug, Clone)]
pub struct JsonbPathLeaf {
    /// Pre-rendered SQL expression, e.g. `(col->'a'->>'b')::int`.
    ///
    /// Constructed only by `JsonbPathRef::build_leaf_condition`, which
    /// validates each segment before building the string. The emitter
    /// pushes this directly via `acc.push_sql`.
    pub expr_sql: String,
    /// The comparison operator.
    pub op: crate::query::condition::LookupOp,
    /// The bound value.
    pub value: FilterValue,
}

// ── Comparison surface for JsonbPathRef<M, V> ─────────────────────────────

use crate::query::field::IntoFilterValue;

impl<M: Model, V: IntoFilterValue + 'static> JsonbPathRef<M, V> {
    /// Build the SQL expression string for this path reference, with the
    /// appropriate type cast for `V`.
    fn expr_sql(self) -> String {
        let type_name = std::any::type_name::<V>();
        let cast = sql_cast_for_type(type_name);
        build_path_sql(self.column, self.path, cast)
    }

    fn leaf_condition(
        self,
        op: crate::query::condition::LookupOp,
        value: FilterValue,
    ) -> Condition {
        Condition::JsonbPath(JsonbPathLeaf {
            expr_sql: self.expr_sql(),
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

    #[test]
    fn sql_cast_for_i32() {
        assert_eq!(sql_cast_for_type("i32"), Some("::int"));
    }

    #[test]
    fn sql_cast_for_i64() {
        assert_eq!(sql_cast_for_type("i64"), Some("::bigint"));
    }

    #[test]
    fn sql_cast_for_string_is_none() {
        assert_eq!(sql_cast_for_type("String"), None);
    }

    #[test]
    fn sql_cast_for_bool() {
        assert_eq!(sql_cast_for_type("bool"), Some("::bool"));
    }
}
