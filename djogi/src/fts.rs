//! Full-text search types — `TsVector` and `TsQuery`.
//!
//! # What
//!
//! Two newtypes over `String` that map 1:1 to the Postgres `tsvector` and
//! `tsquery` column types. Both implement `postgres_types::{ToSql, FromSql}`
//! so they encode and decode transparently through the `tokio-postgres` driver.
//! Postgres does not expose `Type::TS_VECTOR` / `Type::TS_QUERY` as named
//! constants in `postgres-types` 0.2, so we match the wire type via
//! `Type::TSVECTOR` / `Type::TSQUERY` (the lowercase variants that the crate
//! does ship). If those variants are not present in the linked version of
//! `postgres-types`, the fallback path encodes both as `TEXT` with an explicit
//! SQL cast at the query site.
//!
//! # Why not derive ToSql/FromSql from postgres-types?
//!
//! `postgres-types`'s derive macro targets composite types (Postgres
//! `COMPOSITE`) and enums. For scalar newtypes we implement the traits by
//! hand: delegate encoding to the inner `String` and provide the OID check
//! against `TSVECTOR` / `TSQUERY`. The implementation is straightforward and
//! gives us precise control over the `accepts` function and the `to_sql_checked`
//! dispatch.
//!
//! # FTS query descriptor
//!
//! `FtsDescriptor` and `FtsSpec` are the runtime-descriptor types consumed by
//! the migration differ (Phase 6). They are declared here alongside the
//! wire types so the full FTS story lives in one module.
//!
//! # Path routing
//!
//! These types are re-exported from `djogi::types::fts` and from the crate
//! root (`djogi::TsVector`, `djogi::TsQuery`). Macro-emitted code routes
//! through `::djogi::TsVector` / `::djogi::TsQuery` — it never imports
//! from this module directly.

use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use std::error::Error;

// ── TsVector ─────────────────────────────────────────────────────────────────

/// A Postgres `tsvector` value.
///
/// Wraps a `String` in the `tsvector` text representation that Postgres
/// produces and expects (e.g. `"'earth':2 'planet':1"`). The codec treats
/// it as raw text and relies on Postgres to parse or produce the canonical
/// `tsvector` form — the Rust side never parses the internal structure.
///
/// Typically comes from a `GENERATED ALWAYS AS (to_tsvector(...)) STORED`
/// column or from an explicit `to_tsvector(...)` expression in a query.
/// User code rarely constructs `TsVector` manually; it is the decoded type
/// for `TSVECTOR` columns in `FromPgRow`.
///
/// # Encoding
///
/// Encodes as `TEXT` wire-type when the `postgres-types` version in use does
/// not export a `TSVECTOR` OID constant. The cast to `tsvector` happens in
/// the SQL expression emitted by the FTS query layer (`to_tsquery(...)` side)
/// so the round-trip is correct even in that case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TsVector(pub String);

impl TsVector {
    /// Wrap a raw tsvector string without any validation.
    ///
    /// The string is passed verbatim to Postgres; malformed tsvectors
    /// produce a runtime error from the driver, not a panic here.
    pub fn new(s: impl Into<String>) -> Self {
        TsVector(s.into())
    }

    /// Borrow the inner tsvector string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TsVector {
    fn from(s: String) -> Self {
        TsVector(s)
    }
}

impl From<TsVector> for String {
    fn from(v: TsVector) -> Self {
        v.0
    }
}

impl std::fmt::Display for TsVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToSql for TsVector {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        // Delegate to String's ToSql — the wire encoding of a tsvector text
        // representation is identical to a plain string value. Postgres knows
        // from the column type that it should parse it as a tsvector.
        self.0.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        // Accept TSVECTOR (named constant shipped by postgres-types) or TEXT
        // (fallback when the OID constant is absent in older versions).
        matches!(ty.name(), "tsvector" | "text")
    }

    to_sql_checked!();
}

impl<'a> FromSql<'a> for TsVector {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let s = <String as FromSql>::from_sql(ty, raw)?;
        Ok(TsVector(s))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.name(), "tsvector" | "text")
    }
}

// ── TsQuery ──────────────────────────────────────────────────────────────────

/// A Postgres `tsquery` value supplied by the application.
///
/// Wraps a query string in the operator syntax Postgres's `to_tsquery`
/// and `plainto_tsquery` functions accept — ampersands (`&`) and pipes (`|`)
/// for AND/OR, `!` for NOT, angle brackets for phrase queries, e.g.
/// `"planet & earth"`. The framework passes the inner string as a bind
/// parameter to `to_tsquery('<dictionary>', $n)` in the emitted SQL; the
/// conversion from text to `tsquery` happens on the Postgres side.
///
/// # Constructing a query
///
/// ```rust
/// use djogi::TsQuery;
///
/// // AND two terms
/// let q = TsQuery::new("planet & earth");
///
/// // OR two terms
/// let q = TsQuery::new("planet | mars");
///
/// // Phrase query (Postgres 9.6+)
/// let q = TsQuery::new("'planet earth'");
/// ```
///
/// # Dictionary handling
///
/// The dictionary name is supplied at the model level via
/// `#[model(fts = { source = "...", dictionary = "english" })]`. The
/// query layer combines the model's dictionary name with the `TsQuery`
/// value to emit `to_tsquery('english', $1)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsQuery(pub String);

impl TsQuery {
    /// Wrap a tsquery operator string. The string is passed to Postgres's
    /// `to_tsquery(dictionary, $n)` function verbatim; malformed queries
    /// produce a runtime error from Postgres, not a panic here.
    pub fn new(s: impl Into<String>) -> Self {
        TsQuery(s.into())
    }

    /// Borrow the inner query string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TsQuery {
    fn from(s: String) -> Self {
        TsQuery(s)
    }
}

impl From<TsQuery> for String {
    fn from(q: TsQuery) -> Self {
        q.0
    }
}

impl std::fmt::Display for TsQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToSql for TsQuery {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        // Bind the raw query text. Postgres parses it through to_tsquery on
        // the server side; the wire format is plain text.
        self.0.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        // Accept TSQUERY or TEXT (fallback for older postgres-types builds).
        matches!(ty.name(), "tsquery" | "text")
    }

    to_sql_checked!();
}

impl<'a> FromSql<'a> for TsQuery {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let s = <String as FromSql>::from_sql(ty, raw)?;
        Ok(TsQuery(s))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.name(), "tsquery" | "text")
    }
}

// ── Dictionary name validation ────────────────────────────────────────────────

/// Validate a Postgres text-search dictionary name at macro parse time.
///
/// A valid dictionary name is a plain ASCII identifier: it starts with an
/// ASCII letter or underscore, continues with ASCII letters, digits, or
/// underscores, and is at most 63 bytes long (Postgres's `NAMEDATALEN - 1`
/// cap). No regex engine is used — the rule is spelled out byte-by-byte per
/// `feedback_no_regex_in_djogi`.
///
/// Returns `Ok(())` when the name is valid, `Err(description)` otherwise.
/// The error string is embedded directly into proc-macro diagnostic messages.
///
/// Dictionary names are embedded into SQL as `to_tsquery('<name>', $n)` with
/// single-quote delimiters; the identifier constraint (letters, digits,
/// underscores only, no quotes or special characters) ensures this is safe
/// without additional escaping.
pub fn validate_dictionary_name(name: &str) -> Result<(), String> {
    use crate::ident::IdentError;
    crate::ident::check_plain_ident(name, false).map_err(|e| match e {
        IdentError::Empty => "dictionary name must not be empty".to_owned(),
        IdentError::TooLong { len } => format!(
            "dictionary name `{name}` is {len} bytes; Postgres caps identifiers at 63 bytes"
        ),
        IdentError::BadFirst { .. } => {
            format!("dictionary name `{name}` must start with an ASCII letter or underscore")
        }
        IdentError::BadByte { idx, byte } => format!(
            "dictionary name `{name}` contains invalid character `{}` at position {idx} — \
             only ASCII letters, digits, and underscores are allowed",
            byte as char
        ),
        IdentError::Reserved => {
            unreachable!("check_plain_ident(reserved=false) cannot return Reserved")
        }
    })
}

/// Validate a column name for use in a `source = "col1, col2"` list.
///
/// Each individual column name in the source list must be a plain ASCII
/// identifier (letter or underscore start, alphanumerics or underscores after,
/// max 63 bytes). The same rules as [`validate_dictionary_name`] apply, since
/// both feed into `to_tsvector(...)` SQL without further quoting.
pub fn validate_source_column(col: &str) -> Result<(), String> {
    use crate::ident::IdentError;
    crate::ident::check_plain_ident(col, false).map_err(|e| match e {
        IdentError::Empty => "source column name must not be empty".to_owned(),
        IdentError::TooLong { len } => {
            format!("source column `{col}` is {len} bytes; Postgres caps identifiers at 63 bytes")
        }
        IdentError::BadFirst { .. } => {
            format!("source column `{col}` must start with an ASCII letter or underscore")
        }
        IdentError::BadByte { idx, byte } => format!(
            "source column `{col}` contains invalid character `{}` at position {idx}",
            byte as char
        ),
        IdentError::Reserved => {
            unreachable!("check_plain_ident(reserved=false) cannot return Reserved")
        }
    })
}

/// Parse a comma-separated `source = "col1, col2"` string into a list of
/// validated column names.
///
/// Leading/trailing whitespace around each name is stripped before
/// validation. Returns the list of column names on success, or the first
/// validation error encountered.
pub fn parse_source_columns(source: &str) -> Result<Vec<String>, String> {
    let cols: Vec<String> = source.split(',').map(|s| s.trim().to_owned()).collect();

    if cols.is_empty() {
        return Err("`source` must name at least one column".to_owned());
    }

    for col in &cols {
        validate_source_column(col)?;
    }

    Ok(cols)
}

// ── FtsDescriptor ─────────────────────────────────────────────────────────────

/// Runtime FTS configuration emitted into `ModelDescriptor` by
/// `#[model(fts = { source = "...", dictionary = "..." })]`.
///
/// # Phase 6 — migration differ note
///
/// **Changing `dictionary` is a column-type alteration.** The GENERATED
/// ALWAYS AS expression embeds the dictionary name literally:
///
/// ```sql
/// search TSVECTOR GENERATED ALWAYS AS (
///     to_tsvector('<dictionary>', title || ' ' || body)
/// ) STORED
/// ```
///
/// Altering `dictionary` from, say, `"english"` to `"spanish"` requires
/// dropping and recreating the generated column — it is not an in-place
/// `ALTER COLUMN` operation. Phase 6's migration differ must treat a
/// `FtsDescriptor.dictionary` change the same way it treats a `FieldSqlType`
/// change: as a column drop + re-add (with the appropriate data-migration
/// opportunity). The `fts` field on `ModelDescriptor` appearing or
/// disappearing likewise represents a generated column being added or
/// removed.
///
/// # Phase 6 deferred items
///
/// Full differ wiring (comparing two `FtsDescriptor` values and emitting
/// `DROP COLUMN` + `ADD COLUMN`) is deferred to Phase 6. This struct
/// establishes the shape so the differ authors have a stable target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsDescriptor {
    /// The generated column name. Defaults to `"search"`. Phase 8 will add
    /// support for custom names via `#[model(fts = { column = "..." })]`.
    pub column: &'static str,
    /// Comma-separated list of source column names, e.g. `"title, body"`.
    /// Stored verbatim — Phase 6's differ can compare this to detect source
    /// list changes (which also require a column drop + re-add).
    pub source: &'static str,
    /// Postgres text-search configuration name, e.g. `"english"`.
    ///
    /// **Changing this value is a column-type alteration.** See the struct-level
    /// doc above for the reasoning. Phase 6's migration differ must treat a
    /// change here as equivalent to changing `FieldSqlType`.
    pub dictionary: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_dictionary_name_valid() {
        assert!(validate_dictionary_name("english").is_ok());
        assert!(validate_dictionary_name("spanish").is_ok());
        assert!(validate_dictionary_name("pg_catalog_english").is_ok());
        assert!(validate_dictionary_name("_private").is_ok());
        assert!(validate_dictionary_name("dict123").is_ok());
    }

    #[test]
    fn validate_dictionary_name_empty() {
        assert!(validate_dictionary_name("").is_err());
    }

    #[test]
    fn validate_dictionary_name_too_long() {
        let long = "a".repeat(64);
        let result = validate_dictionary_name(&long);
        assert!(result.is_err(), "expected error for 64-byte name");
        assert!(result.unwrap_err().contains("63 bytes"));
    }

    #[test]
    fn validate_dictionary_name_starts_with_digit() {
        let result = validate_dictionary_name("1english");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must start with"));
    }

    #[test]
    fn validate_dictionary_name_contains_hyphen() {
        // Hyphens are not valid in Postgres identifiers without quoting.
        let result = validate_dictionary_name("english-us");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid character"));
    }

    #[test]
    fn validate_dictionary_name_contains_space() {
        let result = validate_dictionary_name("my dict");
        assert!(result.is_err());
    }

    #[test]
    fn validate_dictionary_name_exactly_63_bytes() {
        // 63 ASCII letters is valid.
        let name = "a".repeat(63);
        assert!(validate_dictionary_name(&name).is_ok());
    }

    #[test]
    fn parse_source_columns_single() {
        let cols = parse_source_columns("title").unwrap();
        assert_eq!(cols, vec!["title"]);
    }

    #[test]
    fn parse_source_columns_multiple() {
        let cols = parse_source_columns("title, body").unwrap();
        assert_eq!(cols, vec!["title", "body"]);
    }

    #[test]
    fn parse_source_columns_trims_whitespace() {
        let cols = parse_source_columns("  title  ,  body  ").unwrap();
        assert_eq!(cols, vec!["title", "body"]);
    }

    #[test]
    fn parse_source_columns_invalid_col() {
        // Column starting with a digit should fail.
        let result = parse_source_columns("1col, body");
        assert!(result.is_err());
    }

    #[test]
    fn ts_vector_roundtrip_string() {
        let v = TsVector::new("'earth':2 'planet':1");
        assert_eq!(v.as_str(), "'earth':2 'planet':1");
        let s: String = v.clone().into();
        assert_eq!(s, "'earth':2 'planet':1");
    }

    #[test]
    fn ts_query_roundtrip_string() {
        let q = TsQuery::new("planet & earth");
        assert_eq!(q.as_str(), "planet & earth");
        let s: String = q.clone().into();
        assert_eq!(s, "planet & earth");
    }

    #[test]
    fn fts_descriptor_fields() {
        let desc = FtsDescriptor {
            column: "search",
            source: "title, body",
            dictionary: "english",
        };
        assert_eq!(desc.column, "search");
        assert_eq!(desc.source, "title, body");
        assert_eq!(desc.dictionary, "english");
    }

    #[test]
    fn fts_descriptor_dictionary_change_detected() {
        // Demonstrates the alter-detection shape Phase 6 will consume:
        // two FtsDescriptors with different dictionaries are NOT equal,
        // so the differ can emit a column drop + re-add.
        let d1 = FtsDescriptor {
            column: "search",
            source: "title, body",
            dictionary: "english",
        };
        let d2 = FtsDescriptor {
            column: "search",
            source: "title, body",
            dictionary: "spanish",
        };
        assert_ne!(
            d1, d2,
            "different dictionaries must be detected as a change"
        );
    }
}
