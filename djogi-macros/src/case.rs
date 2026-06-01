//! Shared case-conversion helpers for proc macros.
//! # What
//! Both `#[derive(DjogiEnum)]` and `#[derive(JsonbSchema)]` need to apply
//! `rename_all` rules to Rust identifiers. This module centralises the
//! [`RenameAll`] enum and its byte-level conversion functions so neither
//! derive duplicates them.
//! # Why shared
//! `DjogiEnum` maps PascalCase variant names to Postgres wire strings.
//! `JsonbSchema` maps snake_case field names to JSON object keys when a
//! container-level `#[serde(rename_all = "...")]` attribute is present.
//! Both operations use the same 7 case values and the same conversion
//! functions — pulling them into one place eliminates drift.
//! # No regex
//! All conversions use byte-level predicates (`u8::is_ascii_uppercase`,
//! etc.) and no regex-engine dependency per `feedback_no_regex_in_djogi`.

use proc_macro2::Span;

// ---------------------------------------------------------------------------
// RenameAll
// ---------------------------------------------------------------------------

/// Supported `rename_all` values — shared across `DjogiEnum` and
/// `JsonbSchema` container attributes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum RenameAll {
    #[default]
    SnakeCase,
    ScreamingSnakeCase,
    Lowercase,
    Uppercase,
    PascalCase,
    CamelCase,
    KebabCase,
}

impl RenameAll {
    /// Parse a string literal value into a [`RenameAll`] variant.
    /// Returns a [`syn::Error`] if the string is not one of the seven
    /// supported values. The `span` is attached to the error so the
    /// compiler diagnostic points at the attribute token.
    pub(crate) fn from_str(s: &str, span: Span) -> syn::Result<Self> {
        match s {
            "snake_case" => Ok(RenameAll::SnakeCase),
            "SCREAMING_SNAKE_CASE" => Ok(RenameAll::ScreamingSnakeCase),
            "lowercase" => Ok(RenameAll::Lowercase),
            "UPPERCASE" => Ok(RenameAll::Uppercase),
            "PascalCase" => Ok(RenameAll::PascalCase),
            "camelCase" => Ok(RenameAll::CamelCase),
            "kebab-case" => Ok(RenameAll::KebabCase),
            other => Err(syn::Error::new(
                span,
                format!(
                    "unknown rename_all value `{other}`; expected one of: \
                     snake_case, SCREAMING_SNAKE_CASE, lowercase, UPPERCASE, \
                     PascalCase, camelCase, kebab-case"
                ),
            )),
        }
    }

    /// Apply this case conversion to a **PascalCase** Rust identifier.
    /// Used by `DjogiEnum` which always receives PascalCase variant names
    /// (`InMaintenance`, `Active`, etc.). If you have a snake_case identifier
    /// (struct field names), use [`RenameAll::apply_to_field`] instead.
    /// Input must be a valid Rust identifier (ASCII). Non-ASCII input has
    /// undefined behaviour — out-of-scope per `feedback_no_regex_in_djogi`.
    pub(crate) fn apply(self, name: &str) -> String {
        match self {
            RenameAll::SnakeCase => pascal_to_snake(name),
            RenameAll::ScreamingSnakeCase => pascal_to_snake(name)
                .bytes()
                .map(|b| {
                    if b == b'_' {
                        b'_'
                    } else {
                        b.to_ascii_uppercase()
                    }
                })
                .map(char::from)
                .collect(),
            RenameAll::Lowercase => name.to_ascii_lowercase(),
            RenameAll::Uppercase => name.to_ascii_uppercase(),
            RenameAll::PascalCase => name.to_owned(),
            RenameAll::CamelCase => pascal_to_camel(name),
            RenameAll::KebabCase => pascal_to_snake(name).replace('_', "-"),
        }
    }

    /// Apply this case conversion to a **snake_case** Rust struct field name.
    /// Used by `JsonbSchema` container-level `#[serde(rename_all = "...")]`
    /// where the input is always a snake_case field identifier
    /// (`engine_type`, `weight_kg`, etc.).
    /// The rules differ from [`RenameAll::apply`] for `camelCase` and
    /// `PascalCase`:
    /// - `camelCase`: `engine_type` → `engineType` (via `snake_to_camel`)
    /// - `PascalCase`: `engine_type` → `EngineType` (via `snake_to_pascal`)
    ///   All other rules (`snake_case`, `SCREAMING_SNAKE_CASE`, `lowercase`,
    ///   `UPPERCASE`, `kebab-case`) produce the same output for snake_case input
    ///   as they do for PascalCase input — the byte-level operations are neutral
    ///   to the boundary style.
    ///   Input must be a valid Rust identifier (ASCII). Non-ASCII input has
    ///   undefined behaviour — out-of-scope per `feedback_no_regex_in_djogi`.
    pub(crate) fn apply_to_field(self, name: &str) -> String {
        match self {
            RenameAll::SnakeCase => name.to_owned(),
            RenameAll::ScreamingSnakeCase => name
                .bytes()
                .map(|b| {
                    if b == b'_' {
                        b'_'
                    } else {
                        b.to_ascii_uppercase()
                    }
                })
                .map(char::from)
                .collect(),
            RenameAll::Lowercase => name.to_ascii_lowercase(),
            RenameAll::Uppercase => name.to_ascii_uppercase(),
            RenameAll::PascalCase => snake_to_pascal(name),
            RenameAll::CamelCase => snake_to_camel(name),
            RenameAll::KebabCase => name.replace('_', "-"),
        }
    }
}

// ---------------------------------------------------------------------------
// Case conversion functions
// ---------------------------------------------------------------------------

/// Convert `PascalCase` or `snake_case` → `snake_case`.
/// Inserts `_` before each uppercase letter that is either preceded by a
/// lowercase letter (standard camel boundary, e.g. `fooBar` → `foo_bar`) or
/// followed by a lowercase letter (trailing letter of an all-caps run that
/// starts a new word, e.g. `XMLParser` → `xml_parser`, `HTTPSProxy` →
/// `https_proxy`).
/// For an already-`snake_case` identifier (lowercase letters and underscores
/// only), this is a no-op: no uppercase letters are present, so no underscores
/// are inserted.
/// The leading letter of the identifier never gets a leading underscore
/// regardless of what comes after it.
/// Pure byte-level — no regex, no regex notation. Handles only ASCII as
/// documented in the module-level note.
pub(crate) fn pascal_to_snake(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 4);
    for (i, &b) in bytes.iter().enumerate() {
        let is_upper = b.is_ascii_uppercase();
        if is_upper && i > 0 {
            let prev_is_lower = bytes[i - 1].is_ascii_lowercase();
            let next_is_lower = i + 1 < bytes.len() && bytes[i + 1].is_ascii_lowercase();
            // Boundary rule: preceded by lowercase (standard camel boundary)
            // OR followed by lowercase (trailing cap of an all-caps run that
            // starts a new word). Both branches insert exactly one `_`.
            if prev_is_lower || next_is_lower {
                out.push(b'_');
            }
        }
        out.push(if is_upper { b.to_ascii_lowercase() } else { b });
    }
    String::from_utf8(out).expect("ASCII-only conversion cannot produce invalid UTF-8")
}

/// Convert `PascalCase` → `camelCase`.
/// Lowercase only the first byte; leave the rest unchanged.
pub(crate) fn pascal_to_camel(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let lower: String = c.to_lowercase().collect();
            lower + chars.as_str()
        }
    }
}

/// Convert `snake_case` → `camelCase`.
/// Splits on `_`, capitalises the first letter of each word after the first,
/// then concatenates. The first word stays lowercase.
/// Examples:
/// - `engine_type` → `engineType`
/// - `weight_kg` → `weightKg`
/// - `foo` → `foo` (no underscores, no change)
/// - `already_lower` → `alreadyLower`
///   Pure byte-level — no regex.
pub(crate) fn snake_to_camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut capitalise_next = false;
    for b in name.bytes() {
        if b == b'_' {
            capitalise_next = true;
        } else if capitalise_next {
            out.push(b.to_ascii_uppercase() as char);
            capitalise_next = false;
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Convert `snake_case` → `PascalCase`.
/// Splits on `_`, capitalises the first letter of every word, concatenates.
/// Used by `JsonbSchema` when `rename_all = "PascalCase"` is set on a struct
/// whose fields are in snake_case.
pub(crate) fn snake_to_pascal(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut capitalise_next = true;
    for b in name.bytes() {
        if b == b'_' {
            capitalise_next = true;
        } else if capitalise_next {
            out.push(b.to_ascii_uppercase() as char);
            capitalise_next = false;
        } else {
            out.push(b as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pascal_to_snake ────────────────────────────────────────────────────────

    #[test]
    fn pascal_to_snake_lowercase_only() {
        assert_eq!(pascal_to_snake("Active"), "active");
    }

    #[test]
    fn pascal_to_snake_two_words() {
        assert_eq!(pascal_to_snake("InMaintenance"), "in_maintenance");
        assert_eq!(pascal_to_snake("MyVariantName"), "my_variant_name");
    }

    #[test]
    fn pascal_to_snake_acronym_run() {
        // Fixed boundary rule (commit 07bf473): XMLParser → xml_parser (NOT x_m_l_parser).
        assert_eq!(pascal_to_snake("XMLParser"), "xml_parser");
        assert_eq!(pascal_to_snake("HTTPSProxy"), "https_proxy");
    }

    #[test]
    fn pascal_to_snake_all_caps() {
        assert_eq!(pascal_to_snake("ABC"), "abc");
        assert_eq!(pascal_to_snake("AB"), "ab");
        assert_eq!(pascal_to_snake("A"), "a");
    }

    #[test]
    fn pascal_to_snake_trailing_acronym() {
        assert_eq!(pascal_to_snake("ParserXML"), "parser_xml");
    }

    #[test]
    fn pascal_to_snake_camel_input() {
        assert_eq!(pascal_to_snake("myField"), "my_field");
    }

    #[test]
    fn pascal_to_snake_empty() {
        assert_eq!(pascal_to_snake(""), "");
    }

    #[test]
    fn pascal_to_snake_io_error() {
        assert_eq!(pascal_to_snake("IOError"), "io_error");
    }

    #[test]
    fn pascal_to_snake_already_snake() {
        // snake_case input must be a no-op.
        assert_eq!(pascal_to_snake("engine_type"), "engine_type");
        assert_eq!(pascal_to_snake("weight_kg"), "weight_kg");
        assert_eq!(pascal_to_snake("foo"), "foo");
    }

    // ── snake_to_camel ────────────────────────────────────────────────────────

    #[test]
    fn snake_to_camel_basic() {
        assert_eq!(snake_to_camel("engine_type"), "engineType");
        assert_eq!(snake_to_camel("weight_kg"), "weightKg");
    }

    #[test]
    fn snake_to_camel_no_underscores() {
        assert_eq!(snake_to_camel("foo"), "foo");
    }

    #[test]
    fn snake_to_camel_multiple_words() {
        assert_eq!(snake_to_camel("first_second_third"), "firstSecondThird");
    }

    // ── snake_to_pascal ───────────────────────────────────────────────────────

    #[test]
    fn snake_to_pascal_basic() {
        assert_eq!(snake_to_pascal("engine_type"), "EngineType");
        assert_eq!(snake_to_pascal("weight_kg"), "WeightKg");
    }

    #[test]
    fn snake_to_pascal_single_word() {
        assert_eq!(snake_to_pascal("foo"), "Foo");
    }

    // ── RenameAll::apply (PascalCase input, used by DjogiEnum) ────────────────

    #[test]
    fn rename_all_apply_camel_case_from_pascal() {
        // DjogiEnum calls apply with PascalCase variant names.
        assert_eq!(RenameAll::CamelCase.apply("InMaintenance"), "inMaintenance");
        assert_eq!(RenameAll::CamelCase.apply("Active"), "active");
    }

    #[test]
    fn rename_all_apply_snake_case_from_pascal() {
        assert_eq!(
            RenameAll::SnakeCase.apply("InMaintenance"),
            "in_maintenance"
        );
    }

    // ── RenameAll::apply_to_field (snake_case input, used by JsonbSchema) ─────

    #[test]
    fn rename_all_camel_case_from_snake() {
        assert_eq!(
            RenameAll::CamelCase.apply_to_field("engine_type"),
            "engineType"
        );
        assert_eq!(RenameAll::CamelCase.apply_to_field("weight_kg"), "weightKg");
    }

    #[test]
    fn rename_all_snake_case_is_noop_for_snake() {
        assert_eq!(
            RenameAll::SnakeCase.apply_to_field("engine_type"),
            "engine_type"
        );
    }

    #[test]
    fn rename_all_kebab_from_snake() {
        assert_eq!(
            RenameAll::KebabCase.apply_to_field("engine_type"),
            "engine-type"
        );
        assert_eq!(
            RenameAll::KebabCase.apply_to_field("weight_kg"),
            "weight-kg"
        );
    }

    #[test]
    fn rename_all_screaming_snake_from_snake() {
        assert_eq!(
            RenameAll::ScreamingSnakeCase.apply_to_field("engine_type"),
            "ENGINE_TYPE"
        );
    }

    #[test]
    fn rename_all_pascal_from_snake() {
        assert_eq!(
            RenameAll::PascalCase.apply_to_field("engine_type"),
            "EngineType"
        );
        assert_eq!(
            RenameAll::PascalCase.apply_to_field("weight_kg"),
            "WeightKg"
        );
    }
}
