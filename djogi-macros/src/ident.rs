//! Macro-time validator for user-declared column identifiers.
//!
//! The `#[model]` macro emits `COLUMN_LIST` — an unquoted comma-join of
//! every column name — into `SELECT` and `RETURNING` clauses. For that
//! emission to be safe, every column name must satisfy the Postgres
//! unquoted-identifier contract:
//!
//! 1. Non-empty.
//! 2. Length ≤ 63 bytes (`NAMEDATALEN - 1`).
//! 3. First byte is an ASCII letter or underscore; every remaining byte
//!    is ASCII alphanumeric or underscore.
//! 4. Not a reserved Postgres keyword (case-insensitive).
//!
//! The `#[model]` macro used to reserve only `id` / `created_at` /
//! `updated_at` and accepted any other field name verbatim. A user field
//! like `r#select` (raw Rust keyword escape) stripped to `select` and
//! emitted unquoted produces invalid SQL; a field like `order` or `user`
//! hits the same class of breakage. Validating here — at macro expansion
//! time — turns that silent footgun into a targeted `syn::Error`
//! pointing at the offending field.
//!
//! # Why validate here AND at runtime?
//!
//! `djogi/src/ident.rs` carries the runtime validator — `assert_plain_ident`
//! and `const_assert_plain_ident` — that fires on macro-emitted
//! identifiers reaching `SqlAccumulator::push_sql`. The runtime path
//! catches broken macro emissions or downstream bypass attempts
//! (framework bugs) and panics with an operator-facing message.
//!
//! This macro-time validator catches the dual problem: the *user-facing*
//! case where a valid Rust identifier names an unusable SQL column.
//! Running it in the macro means the compiler reports the problem at the
//! field's source span in the user's code, not as a runtime panic buried
//! in SQL emission.
//!
//! # No regex
//!
//! Per the project-wide rule in `CLAUDE.md` / `docs/spec/decisions.md`,
//! no regex engine is used. The byte-level checks below are pure
//! `u8::is_ascii_alphabetic` / `u8::is_ascii_alphanumeric` + sorted
//! const-slice `binary_search` for the reserved-keyword lookup.
//!
//! # Keyword list scope
//!
//! The list tracks Postgres 18 **fully-reserved** keywords (catcode `R`
//! in `pg_get_keywords()`). Non-reserved and "reserved (can be function
//! or type)" keywords are accepted unquoted by the server and therefore
//! accepted here. The sorting invariant is pinned by
//! [`reserved_keywords_is_sorted_and_lowercase`] in the unit tests.

use syn::spanned::Spanned;

/// Postgres's usable identifier length (NAMEDATALEN - 1 on a default
/// build). Matches the runtime-side constant in `djogi/src/ident.rs`.
const MAX_IDENT_LEN: usize = 63;

/// Postgres 18 fully-reserved keywords (catcode `R`). Lowercase,
/// sorted — `binary_search` depends on both. Mirrors the list in
/// `djogi/src/ident.rs::RESERVED_KEYWORDS`; keep the two in sync.
///
/// Authoritative source: Appendix C (SQL Key Words) of the Postgres
/// 18 manual — entries where the "PostgreSQL" column reads
/// "reserved".
const RESERVED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

/// Validate that every user-declared field on the `struct_item` has a
/// column name that satisfies the Postgres unquoted-identifier contract.
///
/// `struct_item` is the pre-injection struct — framework fields (`id`,
/// `created_at`, `updated_at`) have not been added yet, so every named
/// field corresponds to a user declaration whose span is what a
/// rust-analyzer / compiler diagnostic should underline.
///
/// The check strips the `r#` raw-identifier prefix so `r#type` -> `type`,
/// matching the column-name convention the emitter itself uses. This
/// closes the `r#select` gap: the Rust parser accepts `r#select` as a
/// field ident (keyword escape), but the stringified form is the
/// reserved `select` keyword, which breaks unquoted SQL emission.
pub fn validate_field_column_names(struct_item: &syn::ItemStruct) -> syn::Result<()> {
    let syn::Fields::Named(named) = &struct_item.fields else {
        // Tuple / unit structs are rejected earlier by `inject::validate_shape`
        // with a clearer, top-level error message. Bail silently here so we
        // don't shadow that diagnostic.
        return Ok(());
    };

    for field in &named.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        let raw = ident.to_string();
        let column = raw.strip_prefix("r#").unwrap_or(&raw);
        check_one(column, field.span())?;
    }
    Ok(())
}

/// Run the four-rule validator on a single column name and report a
/// `syn::Error` at the given span on the first rule violation.
///
/// Factored out so the unit tests (and any future callers — projection
/// aliases, renamed_from targets, etc.) can exercise the classifier
/// without constructing a full `ItemStruct`.
pub fn check_one(column: &str, span: proc_macro2::Span) -> syn::Result<()> {
    let bytes = column.as_bytes();

    if bytes.is_empty() {
        return Err(syn::Error::new(
            span,
            "#[model] field name resolves to an empty SQL column name \
             — this is a framework bug; please report it",
        ));
    }

    if bytes.len() > MAX_IDENT_LEN {
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] field name {column:?} is {len} bytes as a SQL column, \
                 exceeding Postgres's {max}-byte usable identifier length \
                 (NAMEDATALEN - 1). Rename the field or use `#[field(renamed_from = \"…\")]` \
                 to map to a shorter column name.",
                len = bytes.len(),
                max = MAX_IDENT_LEN,
            ),
        ));
    }

    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] field name {column:?} starts with a character that cannot \
                 appear at the start of an unquoted Postgres identifier. Use an ASCII \
                 letter or underscore as the first character."
            ),
        ));
    }

    for &byte in &bytes[1..] {
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(syn::Error::new(
                span,
                format!(
                    "#[model] field name {column:?} contains a character that is not \
                     a valid unquoted Postgres identifier byte. Only ASCII alphanumerics \
                     and underscores are permitted after the first character."
                ),
            ));
        }
    }

    // Case-insensitive keyword lookup. Every byte at this point is
    // ASCII alnum or `_`, so an in-place lowercase and a sorted
    // `binary_search` suffice. Heap allocation for the lowercased
    // form is acceptable at macro-expansion time — this runs once
    // per field at compile time.
    let lowered = column.to_ascii_lowercase();
    if RESERVED_KEYWORDS.binary_search(&lowered.as_str()).is_ok() {
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] field name {column:?} is a reserved Postgres keyword and \
                 cannot appear unquoted in generated SQL. Rename the field, or use \
                 `#[field(renamed_from = \"…\")]` to map to a non-reserved column \
                 name. (Note: Rust raw-identifier escapes like `r#select` still \
                 produce the reserved SQL column name `select`.)"
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(name: &str) -> Result<(), String> {
        check_one(name, proc_macro2::Span::call_site()).map_err(|e| e.to_string())
    }

    #[test]
    fn accepts_plain_identifier() {
        assert!(classify("owner_id").is_ok());
        assert!(classify("_internal").is_ok());
        assert!(classify("col1").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert!(classify("").is_err());
    }

    #[test]
    fn rejects_leading_digit() {
        assert!(classify("9col").is_err());
        assert!(classify("1name").is_err());
    }

    #[test]
    fn rejects_non_ascii_alnum_byte() {
        assert!(classify("foo-bar").is_err());
        assert!(classify("foo bar").is_err());
        assert!(classify("café").is_err());
    }

    #[test]
    fn rejects_reserved_keyword_case_insensitively() {
        assert!(classify("select").is_err());
        assert!(classify("SELECT").is_err());
        assert!(classify("Order").is_err());
        assert!(classify("where").is_err());
        assert!(classify("user").is_err());
    }

    #[test]
    fn accepts_non_reserved_lookalike() {
        // "column" is reserved in PG18; "columns" (plural) is not.
        // "type" and "data" are NOT fully-reserved — they appear in
        // many other dialects but PG lists them as "non-reserved".
        assert!(classify("columns").is_ok());
        assert!(classify("type").is_ok());
        assert!(classify("data").is_ok());
    }

    #[test]
    fn rejects_identifier_exceeding_limit() {
        let s = "a".repeat(64);
        assert!(classify(&s).is_err());
    }

    #[test]
    fn accepts_exactly_max_length() {
        let s = "a".repeat(63);
        assert!(classify(&s).is_ok());
    }

    #[test]
    fn reserved_keywords_is_sorted_and_lowercase() {
        // `binary_search` in `check_one` depends on sorted-lowercase
        // ordering. Guard against a later edit that breaks either.
        for pair in RESERVED_KEYWORDS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "RESERVED_KEYWORDS must be sorted: {:?} !< {:?}",
                pair[0],
                pair[1],
            );
        }
        for kw in RESERVED_KEYWORDS {
            assert_eq!(
                kw.to_ascii_lowercase().as_str(),
                *kw,
                "RESERVED_KEYWORDS must be lowercase: {kw:?}"
            );
        }
    }
}
