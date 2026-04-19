//! Crate-private SQL identifier validation.
//!
//! This module is the single source of truth for identifier validation
//! across every djogi SQL emitter. Any macro-emitted `&'static str` that
//! eventually lands inside `sqlx::QueryBuilder::push` — a relation path's
//! `source_column` / `target_table`, a `FieldRef`'s column name, a field
//! descriptor's `name` — goes through [`assert_plain_ident`] first so
//! broken macro emissions and hostile downstream bypass attempts produce
//! a framework-bug panic instead of malformed SQL.
//!
//! The validator enforces the Postgres unquoted-identifier contract:
//!
//! 1. Non-empty.
//! 2. Length ≤ 63 bytes (`NAMEDATALEN - 1`), so Rust-level and
//!    Postgres-level identifier identity cannot diverge through
//!    server-side truncation.
//! 3. First byte is an ASCII letter or underscore; every remaining
//!    byte is ASCII alphanumeric or underscore. Djogi additionally
//!    rejects the `$` byte that Postgres tolerates in unquoted
//!    identifiers, to keep the class trivial to reason about.
//!    (Implementation is pure `u8::is_ascii_alphabetic` /
//!    `u8::is_ascii_alphanumeric` — no regex engine, no dependency.)
//! 4. Not a reserved Postgres keyword (case-insensitive; catcode `R`
//!    in `pg_get_keywords()` as of Postgres 18).
//!
//! Callers that emit literals from `#[derive(Model)]` are the intended
//! audience; the panic messages read as framework bugs or bypass attempts.

/// Postgres's usable identifier length — `NAMEDATALEN - 1` on a default
/// build. Identifiers longer than this are silently truncated by the
/// server, which would let two Rust-distinct identifiers collide after
/// the first 63 bytes. Rejecting up front keeps the Rust- and SQL-level
/// identity contracts aligned.
pub(crate) const MAX_IDENT_LEN: usize = 63;

/// Postgres fully-reserved keywords (catcode `R` in `pg_get_keywords()`
/// as of Postgres 18). These cannot be used as identifiers unless
/// quoted, so emitting them unquoted into `SELECT`, `FROM`, or
/// `LEFT JOIN` produces a parse error — turning a hostile downstream
/// call into a broken-SQL vector. Entries are lowercase and sorted for
/// `binary_search`; [`reserved_keywords_is_sorted_and_lowercase`] pins
/// both invariants in tests so a later edit cannot silently break
/// lookup.
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

/// Validate a macro-emitted identifier against the Postgres unquoted-
/// identifier contract. Panics on the first rule violation. See the
/// module-level doc for the four rules.
///
/// `role` labels the identifier in the panic message (e.g.
/// `"source_column"`, `"table_name"`, `"field_name"`); it is the hook
/// an on-call engineer follows back to the failing emission site.
///
/// Panic (rather than `Result`) is appropriate because reaching the
/// helper with a bad identifier indicates either a broken proc-macro
/// emission or a downstream caller deliberately bypassing the macro-
/// support seal — both are framework-bug or misuse cases that the
/// caller cannot recover from.
#[inline]
pub(crate) fn assert_plain_ident(value: &'static str, role: &'static str) {
    assert!(
        !value.is_empty(),
        "djogi::ident: macro-emitted {role} must not be empty — this is a framework bug"
    );
    assert!(
        value.len() <= MAX_IDENT_LEN,
        "djogi::ident: macro-emitted {role} {value:?} is {len} bytes, exceeding Postgres's \
         {max}-byte usable identifier length (NAMEDATALEN - 1) — either the proc-macro emission \
         is broken or downstream code bypassed the macro-support seal",
        len = value.len(),
        max = MAX_IDENT_LEN,
    );
    let bytes = value.as_bytes();
    assert!(
        bytes[0].is_ascii_alphabetic() || bytes[0] == b'_',
        "djogi::ident: macro-emitted {role} {value:?} must start with a letter or underscore \
         — either the proc-macro emission is broken or downstream code bypassed the \
         macro-support seal"
    );
    for &byte in &bytes[1..] {
        assert!(
            byte.is_ascii_alphanumeric() || byte == b'_',
            "djogi::ident: macro-emitted {role} {value:?} contains a non-identifier character \
             — either the proc-macro emission is broken or downstream code bypassed the \
             macro-support seal"
        );
    }
    // Stack-allocated lowercase for the reserved-keyword lookup. The length
    // check above bounded `value.len()` ≤ 63, so a 64-byte buffer is always
    // sufficient and this path allocates nothing on the heap. Every byte is
    // ASCII alnum or `_` at this point, so `std::str::from_utf8` on the
    // lowercased slice is infallible.
    let mut lower_buf = [0u8; MAX_IDENT_LEN + 1];
    let len = value.len();
    lower_buf[..len].copy_from_slice(bytes);
    lower_buf[..len].make_ascii_lowercase();
    let lower = std::str::from_utf8(&lower_buf[..len])
        .expect("ASCII identifier must be valid UTF-8 after lowercasing");
    assert!(
        RESERVED_KEYWORDS.binary_search(&lower).is_err(),
        "djogi::ident: macro-emitted {role} {value:?} is a reserved Postgres keyword and cannot \
         appear unquoted in generated SQL — either the proc-macro emission is broken or \
         downstream code bypassed the macro-support seal"
    );
}

/// Debug-build-only identifier assertion. Expands to
/// [`assert_plain_ident`] under `cfg(debug_assertions)` and to nothing
/// in release builds, so the check runs in tests and `cargo run` but
/// contributes zero overhead to a production `--release` binary.
///
/// Intended for hot paths that accept a `&'static str` from a source
/// the compiler cannot structurally seal — notably the per-row loops
/// in `relation::prefetch` and `relation::select_related` that read
/// `descriptor().fields[].name`. Sealing the `Model` trait stops a
/// hand-rolled `impl Model` from reaching that code, but a hostile
/// `#[derive(Model)]`-equivalent macro in a downstream crate could
/// still feed the framework a malformed field name; the debug assert
/// turns that into a loud framework-bug panic in CI.
macro_rules! debug_assert_ident {
    ($value:expr, $role:literal) => {{
        #[cfg(debug_assertions)]
        {
            $crate::ident::assert_plain_ident($value, $role);
        }
    }};
}

pub(crate) use debug_assert_ident;

#[cfg(test)]
mod tests {
    use super::*;

    fn try_assert(value: &'static str) -> std::thread::Result<()> {
        std::panic::catch_unwind(|| assert_plain_ident(value, "test_ident"))
    }

    #[test]
    fn accepts_plain_identifier() {
        assert!(try_assert("owner_id").is_ok());
    }

    #[test]
    fn accepts_identifier_with_trailing_digits() {
        assert!(try_assert("col1").is_ok());
        assert!(try_assert("t_abc123").is_ok());
    }

    #[test]
    fn accepts_leading_underscore() {
        // Rust convention for private names; Postgres also accepts.
        assert!(try_assert("_internal").is_ok());
        assert!(try_assert("_").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert!(try_assert("").is_err());
    }

    #[test]
    fn rejects_leading_digit() {
        // Closes the pre-fix hole: the old byte check accepted "123",
        // which would emit `SELECT p.123 ...` or `LEFT JOIN 9table ...`.
        assert!(try_assert("123").is_err());
        assert!(try_assert("9col").is_err());
    }

    #[test]
    fn rejects_reserved_keyword() {
        assert!(try_assert("select").is_err());
        assert!(try_assert("table").is_err());
        assert!(try_assert("where").is_err());
    }

    #[test]
    fn rejects_reserved_keyword_case_insensitively() {
        // Postgres folds unquoted identifiers to lowercase, so
        // "SELECT", "Select", "sElEcT" all reference the SELECT
        // keyword. The validator must match the same case rule.
        assert!(try_assert("SELECT").is_err());
        assert!(try_assert("Where").is_err());
        assert!(try_assert("TaBlE").is_err());
    }

    #[test]
    fn rejects_identifier_exceeding_limit() {
        const LONG: &str = "a234567890123456789012345678901234567890123456789012345678901234";
        assert_eq!(LONG.len(), 64);
        assert!(try_assert(LONG).is_err());
    }

    #[test]
    fn accepts_identifier_at_exactly_63_bytes() {
        const AT_LIMIT: &str = "a23456789012345678901234567890123456789012345678901234567890123";
        assert_eq!(AT_LIMIT.len(), 63);
        assert!(try_assert(AT_LIMIT).is_ok());
    }

    #[test]
    fn rejects_metacharacter_payload() {
        // The original SQL-injection shape from the Task 4 seal fixture.
        assert!(try_assert("owner_id) OR 1=1 --").is_err());
    }

    #[test]
    fn rejects_nul_byte() {
        // Explicit pin for behavior on embedded NUL bytes — these are
        // not alphanumeric or underscore and are rejected on the first
        // per-byte check. Documents the current behavior so a later
        // refactor can't silently regress.
        assert!(try_assert("a\0b").is_err());
    }

    #[test]
    fn rejects_non_ascii_alpha() {
        // Unicode alphabetic chars are not ASCII alphanumeric — wholesale
        // rejected on the per-byte check. Covers the "mixed-script
        // confusable" attack surface.
        assert!(try_assert("café").is_err());
        assert!(try_assert("naïve").is_err());
    }

    #[test]
    fn reserved_keywords_is_sorted_and_lowercase() {
        // binary_search in assert_plain_ident assumes sorted, lowercase
        // entries. Guard against a later edit that adds keywords out of
        // order or in uppercase (which would silently fail to match the
        // lowercased-input lookup).
        for pair in RESERVED_KEYWORDS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "RESERVED_KEYWORDS must be sorted for binary_search: {:?} !< {:?}",
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

    #[test]
    fn debug_assert_ident_matches_runtime_validator() {
        // Under `cfg(test)`, `debug_assertions` is on, so the macro must
        // panic identically to `assert_plain_ident`. This pins the macro's
        // behavior against the validator so drift would fail.
        let caught = std::panic::catch_unwind(|| debug_assert_ident!("select", "field_name"));
        assert!(
            caught.is_err(),
            "debug_assert_ident should panic on reserved keyword"
        );
    }

    #[test]
    fn debug_assert_ident_accepts_valid_name() {
        // Paired positive case — confirms the macro is not simply
        // panicking unconditionally and keeps the hot path quiet for
        // well-formed identifiers.
        let ok = std::panic::catch_unwind(|| debug_assert_ident!("owner_id", "field_name"));
        assert!(ok.is_ok());
    }
}
