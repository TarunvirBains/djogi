//! Macro-time validator for user-declared column identifiers.
//! The `#[model]` macro emits `COLUMN_LIST` — an unquoted comma-join of
//! every column name — into `SELECT` and `RETURNING` clauses. For that
//! emission to be safe, every column name must satisfy the Postgres
//! unquoted-identifier contract:
//! 1. Non-empty.
//! 2. Length ≤ 63 bytes (`NAMEDATALEN - 1`).
//! 3. First byte is an ASCII letter or underscore; every remaining byte
//! is ASCII alphanumeric or underscore.
//! 4. Not a reserved Postgres keyword (case-insensitive).
//! The `#[model]` macro used to reserve only `id` / `created_at` /
//! `updated_at` and accepted any other field name verbatim. A user field
//! like `r#select` (raw Rust keyword escape) stripped to `select` and
//! emitted unquoted produces invalid SQL; a field like `order` or `user`
//! hits the same class of breakage. Validating here — at macro expansion
//! time — turns that silent footgun into a targeted `syn::Error`
//! pointing at the offending field.
//! # Why validate here AND at runtime?
//! `djogi/src/ident.rs` carries the runtime validator — `assert_plain_ident`
//! and `const_assert_plain_ident` — that fires on macro-emitted
//! identifiers reaching `SqlAccumulator::push_sql`. The runtime path
//! catches broken macro emissions or downstream bypass attempts
//! (framework bugs) and panics with an operator-facing message.
//! This macro-time validator catches the dual problem: the *user-facing*
//! case where a valid Rust identifier names an unusable SQL column.
//! Running it in the macro means the compiler reports the problem at the
//! field's source span in the user's code, not as a runtime panic buried
//! in SQL emission.
//! # Framework-reserved `__djogi_` prefix
//! In addition to the four byte-shape rules, this validator rejects
//! identifiers that enter djogi's reserved namespace (`__djogi_*`).
//! That namespace is used for framework-internal recursive CTE names,
//! derived-table aliases, and synthetic column slots; an adopter-
//! declared field or table starting with `__djogi_` would shadow them
//! at SQL emission time. The reservation is enforced uniformly across
//! every adopter-facing entry point — this macro-time gate, plus the
//! runtime helpers in `djogi/src/ident.rs::check_user_supplied_ident` /
//! `assert_user_supplied_ident`. See `docs/spec/reserved-identifiers.md`
//! for the inventory.
//! # No regex
//! Per the project-wide rule in `CLAUDE.md` / `docs/spec/decisions.md`,
//! no regex engine is used. The byte-level checks below are pure
//! `u8::is_ascii_alphabetic` / `u8::is_ascii_alphanumeric` + sorted
//! const-slice `binary_search` for the reserved-keyword lookup.
//! # Keyword list scope
//! The list tracks Postgres 18 **fully-reserved** keywords (catcode `R`
//! in `pg_get_keywords()`). Non-reserved and "reserved (can be function
//! or type)" keywords are accepted unquoted by the server and therefore
//! accepted here. The sorting invariant is pinned by
//! [`reserved_keywords_is_sorted_and_lowercase`] in the unit tests.

use syn::spanned::Spanned;

/// Postgres's usable identifier length (NAMEDATALEN - 1 on a default
/// build). Matches the runtime-side constant in `djogi/src/ident.rs`.
const MAX_IDENT_LEN: usize = 63;

/// Framework-reserved identifier prefix.
/// Mirrors `RESERVED_DJOGI_PREFIX` in `djogi/src/ident.rs`; keep the
/// two in sync (the rule is the same on both sides of the macro
/// boundary). The match is ASCII-case-insensitive because Postgres
/// folds unquoted identifiers to lowercase.
const RESERVED_DJOGI_PREFIX: &[u8] = b"__djogi_";

fn starts_with_reserved_djogi_prefix(bytes: &[u8]) -> bool {
    if bytes.len() < RESERVED_DJOGI_PREFIX.len() {
        return false;
    }
    for i in 0..RESERVED_DJOGI_PREFIX.len() {
        if bytes[i].to_ascii_lowercase() != RESERVED_DJOGI_PREFIX[i] {
            return false;
        }
    }
    true
}

/// Postgres 18 fully-reserved keywords (catcode `R`). Lowercase,
/// sorted — `binary_search` depends on both. Mirrors the list in
/// `djogi/src/ident.rs::RESERVED_KEYWORDS`; keep the two in sync.
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
/// `struct_item` is the pre-injection struct — framework fields (`id`,
/// `created_at`, `updated_at`) have not been added yet, so every named
/// field corresponds to a user declaration whose span is what a
/// rust-analyzer / compiler diagnostic should underline.
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
        if field.ident.is_none() {
            continue;
        }
        let column = crate::syn_util::column_name_from_field(field);
        check_one(&column, field.span())?;
    }
    Ok(())
}

/// Run the four-rule validator on a single column name and report a
/// `syn::Error` at the given span on the first rule violation.
/// Factored out so the unit tests (and any future callers — projection
/// aliases, renamed_from targets, etc.) can exercise the classifier
/// without constructing a full `ItemStruct`.
pub fn check_one(column: &str, span: proc_macro2::Span) -> syn::Result<()> {
    check_ident("field name", column, span)
}

/// Run the same four-rule validator on a `#[model(table = "...")]`
/// value. 's `OuterRef::as_qualified_expr` pushes
/// `Model::table_name()` directly into emitted SQL as `<table>.<col>`;
/// every historical `FROM <table>` emission does the same. Without
/// parse-time validation, a hostile `table = "foo; DROP TABLE x; --"`
/// would smuggle arbitrary SQL into rendered output. Reusing the
/// column-name validator keeps the rules identical (the SQL emitter's
/// safety contract is the same on both sides) while differentiating
/// the diagnostic wording so errors point at the correct attribute.
pub fn check_table_name(table: &str, span: proc_macro2::Span) -> syn::Result<()> {
    check_ident("table name", table, span)
}

/// Run the byte-shape validator on a `#[field(domain = "...")]` value
/// Piece A.
/// Domain names are Postgres SQL type identifiers (`CREATE DOMAIN
/// <name> AS <base>`); the macro emits them verbatim into the column-
/// type slot of generated DDL. The validation is the byte-shape subset
/// of [`check_table_name`] / [`check_one`]:
/// 1. Non-empty.
/// 2. Length ≤ 63 bytes (`NAMEDATALEN - 1`).
/// 3. First byte is an ASCII letter or underscore; every remaining byte
/// is ASCII alphanumeric or underscore.
/// The reserved-keyword check and the framework-reserved `__djogi_`
/// prefix check are intentionally NOT applied: domain identifiers are
/// SQL type names, not column / table identifiers, and `domain = "text"`
/// is a legitimate (if confusing) Postgres declaration. The
/// `__djogi_` prefix likewise has no SQL-namespace collision risk on a
/// domain name because djogi never emits its own domain identifiers
/// Piece A only references adopter-managed domains.
/// Schema-qualified names (`"public.positive_amount"`) are rejected by
/// the byte-shape rule (the `.` is not an ASCII alnum / underscore
/// byte) and are out of Piece A scope. Adopters needing them fall back
/// to `FieldSqlType::Custom("public.positive_amount")` until Piece B.
pub fn check_domain_name(name: &str, span: proc_macro2::Span) -> syn::Result<()> {
    let bytes = name.as_bytes();

    if bytes.is_empty() {
        return Err(syn::Error::new(
            span,
            "domain name must be a valid unquoted Postgres identifier: \
    ASCII letter or underscore as first character, ASCII \
    alphanumerics or underscores only after, at most 63 bytes",
        ));
    }

    if bytes.len() > MAX_IDENT_LEN {
        return Err(syn::Error::new(
            span,
            "domain name must be a valid unquoted Postgres identifier: \
    ASCII letter or underscore as first character, ASCII \
    alphanumerics or underscores only after, at most 63 bytes",
        ));
    }

    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(syn::Error::new(
            span,
            "domain name must be a valid unquoted Postgres identifier: \
    ASCII letter or underscore as first character, ASCII \
    alphanumerics or underscores only after, at most 63 bytes",
        ));
    }

    for &byte in &bytes[1..] {
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(syn::Error::new(
                span,
                "domain name must be a valid unquoted Postgres identifier: \
     ASCII letter or underscore as first character, ASCII \
     alphanumerics or underscores only after, at most 63 bytes",
            ));
        }
    }

    Ok(())
}

fn check_ident(kind: &str, value: &str, span: proc_macro2::Span) -> syn::Result<()> {
    let bytes = value.as_bytes();

    if bytes.is_empty() {
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] {kind} resolves to an empty SQL identifier \
     — this is a framework bug; please report it"
            ),
        ));
    }

    if bytes.len() > MAX_IDENT_LEN {
        let rename_hint = match kind {
            "field name" => {
                " Rename the field or use `#[field(renamed_from = \"…\")]` to map to a shorter column name."
            }
            _ => " Use a shorter name.",
        };
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] {kind} {value:?} is {len} bytes as a SQL identifier, \
     exceeding Postgres's {max}-byte usable identifier length \
     (NAMEDATALEN - 1).{rename_hint}",
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
                "#[model] {kind} {value:?} starts with a character that cannot \
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
                    "#[model] {kind} {value:?} contains a character that is not \
      a valid unquoted Postgres identifier byte. Only ASCII alphanumerics \
      and underscores are permitted after the first character."
                ),
            ));
        }
    }

    // Framework-reserved-prefix block. djogi reserves the `__djogi_`
    // namespace for recursive CTE names, derived-table aliases, and
    // synthetic column slots; a user-declared field or table in that
    // namespace would shadow the framework's own emissions at SQL
    // build time. Apply uniformly to both `field name` and `table
    // name` since both end up unquoted in djogi-emitted SQL. See
    // `docs/spec/reserved-identifiers.md` for the inventory.
    if starts_with_reserved_djogi_prefix(bytes) {
        let rename_hint = match kind {
            "field name" => {
                "Rename the Rust field to a non-reserved column name; use `#[field(renamed_from = \"…\")]` only when migrating from an existing old column."
            }
            _ => "Use a non-reserved name.",
        };
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] {kind} {value:?} starts with the framework-reserved `__djogi_` \
     prefix. djogi uses this namespace for recursive CTE names, derived-table \
     aliases, and synthetic column slots; user-declared identifiers in this \
     namespace would shadow framework-internal emissions. {rename_hint}"
            ),
        ));
    }

    // Case-insensitive keyword lookup. The reserved-keyword table is
    // sorted lowercase; if `value` is already all-lowercase (the common
    // case for snake_case Rust idents) we search it directly, otherwise
    // we lowercase once and search the owned form.
    let is_already_lowercase = bytes.iter().all(|b| !b.is_ascii_uppercase());
    let is_reserved = if is_already_lowercase {
        RESERVED_KEYWORDS.binary_search(&value).is_ok()
    } else {
        let lowered = value.to_ascii_lowercase();
        RESERVED_KEYWORDS.binary_search(&lowered.as_str()).is_ok()
    };
    if is_reserved {
        let rename_hint = match kind {
            "field name" => {
                "Rename the Rust field to a non-reserved column name; use `#[field(renamed_from = \"…\")]` only when migrating from an existing old column."
            }
            _ => "Use a non-reserved name.",
        };
        return Err(syn::Error::new(
            span,
            format!(
                "#[model] {kind} {value:?} is a reserved Postgres keyword and \
     cannot appear unquoted in generated SQL. {rename_hint} \
     (Note: Rust raw-identifier escapes like `r#select` still \
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

    // ── Framework-reserved `__djogi_` prefix (issue #82) ─────────────────────

    #[test]
    fn rejects_djogi_reserved_prefix_on_field_name() {
        // The macro-time validator must reject any user-declared field
        // whose column name enters djogi's reserved namespace. Check
        // both bare `__djogi_` and a populated suffix shape so a
        // partial fix that only matches "exactly __djogi_" is caught.
        let err = classify("__djogi_q").expect_err("__djogi_q must be rejected at macro time");
        assert!(
            err.contains("framework-reserved `__djogi_` prefix"),
            "expected reserved-prefix diagnostic, got: {err}"
        );
        let err = classify("__djogi_anything").expect_err("__djogi_anything must be rejected");
        assert!(
            err.contains("framework-reserved `__djogi_` prefix"),
            "got: {err}"
        );
        let err = classify("__djogi_").expect_err("bare `__djogi_` must be rejected");
        assert!(
            err.contains("framework-reserved `__djogi_` prefix"),
            "got: {err}"
        );
        let err = classify("__DJOGI_q").expect_err("__DJOGI_q must be rejected");
        assert!(
            err.contains("framework-reserved `__djogi_` prefix"),
            "got: {err}"
        );
        let err = classify("__Djogi_q").expect_err("__Djogi_q must be rejected");
        assert!(
            err.contains("framework-reserved `__djogi_` prefix"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_djogi_reserved_prefix_on_table_name() {
        // `check_table_name` reuses `check_ident` so the reservation
        // applies to `#[model(table = "...")]` values too. Pin both
        // entry points so a future refactor that splits them keeps
        // the rule on both.
        let err = check_table_name("__djogi_audit_log", proc_macro2::Span::call_site())
            .expect_err("__djogi_audit_log must be rejected as a table name");
        let msg = err.to_string();
        assert!(
            msg.contains("framework-reserved `__djogi_` prefix"),
            "got: {msg}"
        );
        let err = check_table_name("__DJOGI_audit_log", proc_macro2::Span::call_site())
            .expect_err("__DJOGI_audit_log must be rejected as a table name");
        let msg = err.to_string();
        assert!(
            msg.contains("framework-reserved `__djogi_` prefix"),
            "got: {msg}"
        );
    }

    #[test]
    fn djogi_prefix_diagnostic_carries_rename_hint_for_field_name() {
        // Diagnostic quality — adopters seeing the reservation error
        // benefit from being pointed at the safe migration shape:
        // rename the Rust field now, and use `renamed_from` only to
        // describe the old column that already exists on disk.
        let err = classify("__djogi_legacy_id").expect_err("must reject");
        assert!(
            err.contains("#[field(renamed_from"),
            "field-name diagnostic should include a rename hint, got: {err}"
        );
    }

    #[test]
    fn accepts_non_reserved_lookalikes() {
        // Single-underscore + `djogi` (the notify channel-name shape
        // `djogi_<table>` flows through a derived path, not this
        // validator, but the analogous user-named shape is still
        // accepted) and `_djogi_*` (one underscore) must NOT be
        // rejected — the rule is the exact double-underscore prefix.
        assert!(classify("djogi_audit_log").is_ok());
        assert!(classify("_djogi_x").is_ok());
        assert!(classify("djogi").is_ok());
        // Adopter-owned reserved namespaces (a derivative crate's own
        // `__myprefix_*` reservation) are unaffected.
        assert!(classify("__myprefix_x").is_ok());
    }

    #[test]
    fn reserved_djogi_prefix_constant_matches_runtime_constant() {
        // The macro-time and runtime constants must agree byte-for-byte
        // they encode the same public stability contract documented
        // in `docs/spec/reserved-identifiers.md`. A drift here would
        // produce a class of identifiers that pass macro expansion
        // but blow up at runtime (or vice versa).
        assert_eq!(RESERVED_DJOGI_PREFIX, b"__djogi_");
        assert_eq!(RESERVED_DJOGI_PREFIX.len(), 8);
    }

    // ── Piece A — `check_domain_name` validator ──────────────────
    // Domain names follow the byte-shape subset of the column-name
    // validator: non-empty, ≤63 bytes, ASCII-letter-or-underscore
    // first, ASCII-alphanumeric-or-underscore after. The
    // reserved-keyword and `__djogi_`-prefix checks are deliberately
    // NOT applied — domain identifiers are SQL type names, not
    // column / table identifiers, and `domain = "text"` (a domain
    // that happens to shadow the built-in `text` type) is
    // legitimately legal SQL.

    fn classify_domain(name: &str) -> Result<(), String> {
        check_domain_name(name, proc_macro2::Span::call_site()).map_err(|e| e.to_string())
    }

    #[test]
    fn domain_name_accepts_plain_identifier() {
        assert!(classify_domain("positive_amount").is_ok());
        assert!(classify_domain("Email_Address").is_ok());
        assert!(classify_domain("_private").is_ok());
        assert!(classify_domain("v1").is_ok());
        // Single-character names are valid Postgres identifiers.
        assert!(classify_domain("a").is_ok());
        assert!(classify_domain("_").is_ok());
    }

    #[test]
    fn domain_name_rejects_empty() {
        assert!(classify_domain("").is_err());
    }

    #[test]
    fn domain_name_rejects_leading_digit() {
        assert!(classify_domain("9domain").is_err());
        assert!(classify_domain("123bad").is_err());
    }

    #[test]
    fn domain_name_rejects_non_ascii_alnum_byte() {
        // Hyphen, space, dot — none are ASCII alnum / underscore.
        // The dot rejection is what blocks schema-qualified names like
        // `"public.positive_amount"` at Piece A; adopters needing those
        // fall back to `Custom("public.positive_amount")` per the
        // route's Risk #5.
        assert!(classify_domain("foo-bar").is_err());
        assert!(classify_domain("foo bar").is_err());
        assert!(classify_domain("public.positive_amount").is_err());
        assert!(classify_domain("café").is_err());
    }

    #[test]
    fn domain_name_rejects_identifier_exceeding_limit() {
        let s = "a".repeat(64);
        assert!(classify_domain(&s).is_err());
    }

    #[test]
    fn domain_name_accepts_exactly_max_length() {
        let s = "a".repeat(63);
        assert!(classify_domain(&s).is_ok());
    }

    #[test]
    fn domain_name_accepts_postgres_reserved_keywords() {
        // Domain identifiers shadow built-in SQL type names without
        // collision (every reference appears in `CREATE DOMAIN <name>
        // AS <base>` or column-type position, not in identifier
        // position). `domain = "text"`, `domain = "integer"`, etc.
        // are confusing but legal — accept them.
        assert!(classify_domain("text").is_ok());
        assert!(classify_domain("integer").is_ok());
        assert!(classify_domain("select").is_ok());
        assert!(classify_domain("user").is_ok());
        assert!(classify_domain("order").is_ok());
    }

    #[test]
    fn domain_name_accepts_djogi_reserved_prefix() {
        // The `__djogi_` namespace reservation applies to column /
        // table identifiers (where djogi emits its own synthetic
        // names that would shadow adopter-declared ones). Domain
        // identifiers do not collide with that namespace — djogi
        // never emits its own domains in Piece A — so the prefix
        // check does not run on `check_domain_name`.
        assert!(classify_domain("__djogi_positive_amount").is_ok());
        assert!(classify_domain("__djogi_").is_ok());
    }

    #[test]
    fn domain_name_diagnostic_carries_byte_shape_rule() {
        // The error message must spell out the byte-shape rule so
        // adopters can fix the declaration without consulting the
        // Postgres docs. Per CLAUDE.md `feedback_no_regex_in_djogi`,
        // the rule is stated in plain English, not as regex notation.
        let err = classify_domain("").expect_err("empty must be rejected");
        assert!(
            err.contains("ASCII letter or underscore"),
            "diagnostic should describe the byte-shape rule, got: {err}"
        );
        let err = classify_domain("123bad").expect_err("digit-first must be rejected");
        assert!(
            err.contains("ASCII letter or underscore"),
            "diagnostic should describe the byte-shape rule, got: {err}"
        );
    }
}
