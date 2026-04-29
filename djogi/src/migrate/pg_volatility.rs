//! Postgres 18 built-in default-expression volatility lookup.
//!
//! The online-safety classifier walks `DEFAULT` expressions on
//! [`crate::migrate::SchemaOperation::AddColumn`] / `AlterColumn`
//! operations and asks "would Pg18 catalog-fast-path this default,
//! or does it require a 3-step ExpandContract pattern?". The answer
//! follows Postgres' own `provolatile` category for the function
//! reference inside the expression — `IMMUTABLE` and `STABLE`
//! expressions catalog-fast-path; `VOLATILE` ones force a backfill
//! per Pg18 release notes (catalog-only fast-path is gated on the
//! default being non-volatile).
//!
//! # Why a static table
//!
//! The classifier is required to be **pure** — given the same two
//! descriptor states it must always produce the same classification
//! (per §7 plan and `feedback_completionist_lens.md`). Reading
//! `pg_proc.provolatile` from a live database would push host
//! variability into compose-time output: the same migration source
//! could classify differently depending on which Pg version, which
//! extensions, or which contrib functions happen to be installed on
//! the operator's machine. That is the kind of latent inconsistency
//! that makes `git diff schema_snapshot.json` reviews unreliable.
//!
//! Instead, this module ships a Djogi-owned static slice of every
//! Pg18 built-in identifier the classifier needs to recognise.
//! Lookup is a simple `binary_search` against the sorted slice. The
//! representation is deliberate: a sorted const slice is host-stable
//! (no per-machine variation), zero-allocation (no `HashMap`
//! initialisation), and the sortedness invariant is asserted in a
//! unit test so additions cannot silently break the binary search.
//!
//! # Conservative fallback
//!
//! Identifiers absent from the table — user-defined functions,
//! extension-shipped helpers Djogi cannot enumerate — classify as
//! [`Volatility::Volatile`]. That is the safe direction: an unknown
//! identifier routed through the 3-step ExpandContract path is
//! slower than necessary; the inverse mistake (assuming `IMMUTABLE`
//! when the function is in fact `VOLATILE`) would silently emit a
//! catalog-only `ADD COLUMN ... DEFAULT <volatile>()` that Postgres
//! accepts but the classifier promised would not require backfill,
//! producing the wrong runtime behaviour.
//!
//! Adopters with a known-safe UDF override per-field via
//! `#[field(default_volatility = "stable" | "immutable" | "volatile")]`
//! (parsed by Phase 7.5 T3 — see
//! [`crate::descriptor::DefaultVolatility`]).
//!
//! # §7 routing
//!
//! - `IMMUTABLE` / `STABLE` defaults: `OnlineSafe` (Pg18 catalog-only
//!   fast-path).
//! - `VOLATILE` defaults: `ExpandContract` (3-step pattern — add
//!   nullable column with no default → SET DEFAULT → chunked
//!   backfill).
//!
//! See `docs/superpowers/plans/2026-04-23-phase7-5-live-migrations-and-protected-data-v3.md`
//! §7 (the classification table) for the full routing matrix.

/// Postgres `provolatile` category lifted into Rust.
///
/// Mirrors the Pg18 `pg_proc.provolatile` axis; the variant order
/// matches Postgres' own ordering of "least to most variable".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Volatility {
    /// Pure function — no DB reads, no side effects, same input
    /// always produces same output. Catalog-only fast-path.
    Immutable,
    /// Consistent within one query / statement; consults DB state
    /// but does not modify it. Catalog-only fast-path.
    Stable,
    /// Value can change on every call. Forces 3-step ExpandContract.
    Volatile,
}

/// Sorted slice of `(identifier, volatility)` pairs for every Pg18
/// built-in the classifier needs to recognise.
///
/// **Sort invariant.** Entries are sorted lexicographically by
/// identifier. `binary_search_by_key` depends on this — a unit test
/// asserts the invariant and any future addition must preserve it.
///
/// **Identifier shape.** Built-ins that take no arguments are stored
/// with their parenthesised call form (`now()`, `clock_timestamp()`)
/// because that is the literal text the classifier sees inside a
/// `DEFAULT` expression. SQL keywords that work without parentheses
/// (`CURRENT_TIMESTAMP`, `current_date`, `current_time`,
/// `current_timestamp`) appear in their bare form. The classifier
/// normalises whitespace before lookup but does NOT transform between
/// the two shapes — Postgres treats `now()` and `now` differently
/// (the first is a function call, the second is an identifier
/// reference) and the classifier preserves that distinction.
///
/// **Why both `current_timestamp` cases.** Postgres accepts SQL
/// keywords case-insensitively, so the table holds both forms the
/// classifier is likely to see in a descriptor's `default_sql`
/// expression: the uppercase keyword form `CURRENT_TIMESTAMP` and
/// the lowercase form `current_timestamp` actually emitted by
/// adopters who follow the Djogi convention of lowercase SQL. The
/// classifier compares case-sensitively (via `binary_search_by_key`)
/// rather than performing an `eq_ignore_ascii_case` walk because the
/// sorted-slice + `binary_search` shape costs O(log n) per lookup
/// where a case-folding walk is O(n) — and the few SQL keywords that
/// matter here are short enough that listing both casings keeps the
/// table readable. Function-call forms (`now()`, `random()`) are
/// always lowercase per Postgres' identifier rules, so no parallel
/// uppercase entry is required.
const BUILTIN_VOLATILITY: &[(&str, Volatility)] = &[
    ("CURRENT_TIMESTAMP", Volatility::Stable),
    ("clock_timestamp()", Volatility::Volatile),
    ("current_date", Volatility::Stable),
    ("current_time", Volatility::Stable),
    ("current_timestamp", Volatility::Stable),
    ("gen_random_uuid()", Volatility::Volatile),
    ("now()", Volatility::Stable),
    ("random()", Volatility::Volatile),
    ("statement_timestamp()", Volatility::Stable),
    ("transaction_timestamp()", Volatility::Stable),
];

/// Classify a `DEFAULT` expression's volatility.
///
/// Resolution order:
///
/// 1. Trim ASCII whitespace.
/// 2. Recognise literal shapes — string literals (`'...'` /
///    `E'...'` / dollar-quoted), numeric literals, boolean
///    literals (`true` / `false`), and `NULL`. Literals are pure
///    constants with no runtime evaluation; classify as
///    [`Volatility::Immutable`].
/// 3. Look up the trimmed expression against [`BUILTIN_VOLATILITY`]
///    via `binary_search_by_key`. A match returns the catalog
///    category.
/// 4. Fall through to [`Volatility::Volatile`] — the conservative
///    default. Per the module-level docs, unknown identifiers must
///    route through the 3-step ExpandContract path because we cannot
///    prove they are safe to catalog-fast-path.
///
/// The classifier never reads `pg_catalog`. Compose stays pure.
///
/// # Examples
///
/// ```ignore
/// use djogi::migrate::pg_volatility::{Volatility, classify_default_expression};
///
/// assert_eq!(classify_default_expression("'hello'"), Volatility::Immutable);
/// assert_eq!(classify_default_expression("42"), Volatility::Immutable);
/// assert_eq!(classify_default_expression("true"), Volatility::Immutable);
/// assert_eq!(classify_default_expression("NULL"), Volatility::Immutable);
/// assert_eq!(classify_default_expression("now()"), Volatility::Stable);
/// assert_eq!(classify_default_expression("clock_timestamp()"), Volatility::Volatile);
/// assert_eq!(classify_default_expression("myapp_helper()"), Volatility::Volatile);
/// ```
pub fn classify_default_expression(expr: &str) -> Volatility {
    let trimmed = expr.trim();

    if is_literal_shape(trimmed) {
        return Volatility::Immutable;
    }

    if let Ok(idx) = BUILTIN_VOLATILITY.binary_search_by_key(&trimmed, |(name, _)| *name) {
        return BUILTIN_VOLATILITY[idx].1;
    }

    Volatility::Volatile
}

/// Recognise the four literal shapes Postgres accepts in a `DEFAULT`
/// expression: string, numeric, boolean, and `NULL`.
///
/// Plain-English shape rules, implemented with byte-level checks only:
///
/// - **String literal.** First non-whitespace byte is a single quote
///   `'`, OR is `E` / `e` followed immediately by a single quote
///   (Postgres' escape-string syntax), OR is `$` (dollar-quoted
///   strings). Internal escaping is not validated here; the
///   classifier only needs to decide "is this expression a
///   constant?" and any malformed literal would fail later at the
///   `ALTER TABLE` site.
/// - **Numeric literal.** First byte is an ASCII digit, optionally
///   preceded by a single `+` or `-` sign. Bytes after that are
///   either ASCII digits, a single `.`, or one of `e` / `E` for
///   exponent notation. Validation is loose for the same reason —
///   Postgres rejects malformed numbers itself.
/// - **Boolean literal.** Exactly `true` or `false` (case-insensitive,
///   matching Postgres' own behaviour).
/// - **NULL.** Exactly `NULL` or `null` (case-insensitive).
fn is_literal_shape(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    // String literal — `'...'`, `E'...'`, `e'...'`, or `$tag$...$tag$`.
    if bytes[0] == b'\'' || bytes[0] == b'$' {
        return true;
    }
    if bytes.len() >= 2 && (bytes[0] == b'E' || bytes[0] == b'e') && bytes[1] == b'\'' {
        return true;
    }

    // Numeric literal — optional sign + leading digit. The classifier
    // accepts the broader Postgres-numeric grammar (decimals,
    // exponents) by checking only the first one or two bytes; a fully
    // malformed numeric still satisfies "shape of a literal" for
    // classification purposes.
    if bytes[0].is_ascii_digit() {
        return true;
    }
    if (bytes[0] == b'+' || bytes[0] == b'-') && bytes.len() >= 2 && bytes[1].is_ascii_digit() {
        return true;
    }

    // Boolean / NULL — case-insensitive match. The classifier compares
    // against the full token because `t` / `f` and other shorthand
    // forms are not accepted in a `DEFAULT` clause.
    if expr.eq_ignore_ascii_case("true")
        || expr.eq_ignore_ascii_case("false")
        || expr.eq_ignore_ascii_case("null")
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{BUILTIN_VOLATILITY, Volatility, classify_default_expression, is_literal_shape};

    /// The binary-search lookup in [`classify_default_expression`]
    /// requires the table to be sorted lexicographically by
    /// identifier. This invariant is load-bearing — a sort-order
    /// regression silently makes `binary_search` return `Err` for
    /// values that are present in the table.
    #[test]
    fn builtin_volatility_table_is_sorted() {
        for window in BUILTIN_VOLATILITY.windows(2) {
            assert!(
                window[0].0 < window[1].0,
                "BUILTIN_VOLATILITY entries out of order: {:?} should sort before {:?}",
                window[0].0,
                window[1].0,
            );
        }
    }

    /// The binary-search lookup also requires no duplicate keys —
    /// the strict `<` in the sortedness assertion catches duplicates,
    /// but a second test asserts the property explicitly so a future
    /// refactor of the sort assertion (e.g. relaxing to `<=`) cannot
    /// silently introduce duplicates.
    #[test]
    fn builtin_volatility_table_has_no_duplicate_keys() {
        for window in BUILTIN_VOLATILITY.windows(2) {
            assert_ne!(
                window[0].0, window[1].0,
                "BUILTIN_VOLATILITY contains duplicate key {:?}",
                window[0].0
            );
        }
    }

    #[test]
    fn string_literals_classify_as_immutable() {
        assert_eq!(
            classify_default_expression("'hello'"),
            Volatility::Immutable
        );
        assert_eq!(classify_default_expression("''"), Volatility::Immutable);
        assert_eq!(
            classify_default_expression("E'with\\nescape'"),
            Volatility::Immutable
        );
        assert_eq!(
            classify_default_expression("$tag$dollar quoted$tag$"),
            Volatility::Immutable
        );
    }

    #[test]
    fn numeric_literals_classify_as_immutable() {
        assert_eq!(classify_default_expression("0"), Volatility::Immutable);
        assert_eq!(classify_default_expression("42"), Volatility::Immutable);
        assert_eq!(classify_default_expression("-7"), Volatility::Immutable);
        assert_eq!(classify_default_expression("+1"), Volatility::Immutable);
        assert_eq!(classify_default_expression("3.14"), Volatility::Immutable);
        assert_eq!(classify_default_expression("1e10"), Volatility::Immutable);
    }

    #[test]
    fn boolean_literals_classify_as_immutable() {
        assert_eq!(classify_default_expression("true"), Volatility::Immutable);
        assert_eq!(classify_default_expression("TRUE"), Volatility::Immutable);
        assert_eq!(classify_default_expression("false"), Volatility::Immutable);
        assert_eq!(classify_default_expression("False"), Volatility::Immutable);
    }

    #[test]
    fn null_literal_classifies_as_immutable() {
        assert_eq!(classify_default_expression("NULL"), Volatility::Immutable);
        assert_eq!(classify_default_expression("null"), Volatility::Immutable);
        assert_eq!(classify_default_expression("Null"), Volatility::Immutable);
    }

    #[test]
    fn stable_builtins_classify_as_stable() {
        assert_eq!(classify_default_expression("now()"), Volatility::Stable);
        assert_eq!(
            classify_default_expression("CURRENT_TIMESTAMP"),
            Volatility::Stable
        );
        assert_eq!(
            classify_default_expression("current_timestamp"),
            Volatility::Stable
        );
        assert_eq!(
            classify_default_expression("current_date"),
            Volatility::Stable
        );
        assert_eq!(
            classify_default_expression("current_time"),
            Volatility::Stable
        );
        assert_eq!(
            classify_default_expression("statement_timestamp()"),
            Volatility::Stable
        );
        assert_eq!(
            classify_default_expression("transaction_timestamp()"),
            Volatility::Stable
        );
    }

    #[test]
    fn volatile_builtins_classify_as_volatile() {
        assert_eq!(
            classify_default_expression("clock_timestamp()"),
            Volatility::Volatile
        );
        assert_eq!(
            classify_default_expression("random()"),
            Volatility::Volatile
        );
        assert_eq!(
            classify_default_expression("gen_random_uuid()"),
            Volatility::Volatile
        );
    }

    #[test]
    fn unknown_identifiers_default_to_volatile() {
        // User-defined functions, extension calls, anything not in the
        // built-in table — must route through the conservative path.
        assert_eq!(
            classify_default_expression("myapp_helper()"),
            Volatility::Volatile
        );
        assert_eq!(
            classify_default_expression("custom_seq_next()"),
            Volatility::Volatile
        );
        assert_eq!(
            classify_default_expression("extension_func(arg)"),
            Volatility::Volatile
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(classify_default_expression("  now()  "), Volatility::Stable);
        assert_eq!(
            classify_default_expression("\tnull\n"),
            Volatility::Immutable
        );
        assert_eq!(
            classify_default_expression("  clock_timestamp()  "),
            Volatility::Volatile
        );
    }

    #[test]
    fn empty_expression_classifies_as_volatile() {
        // Empty string is not a literal and not in the table; the
        // conservative fallback applies. (Empty default is a
        // malformed declaration anyway — this test only pins the
        // observable behaviour.)
        assert_eq!(classify_default_expression(""), Volatility::Volatile);
        assert_eq!(classify_default_expression("   "), Volatility::Volatile);
    }

    #[test]
    fn is_literal_shape_recognises_strings() {
        assert!(is_literal_shape("'foo'"));
        assert!(is_literal_shape("E'foo'"));
        assert!(is_literal_shape("e'foo'"));
        assert!(is_literal_shape("$tag$foo$tag$"));
    }

    #[test]
    fn is_literal_shape_recognises_numbers() {
        assert!(is_literal_shape("0"));
        assert!(is_literal_shape("123"));
        assert!(is_literal_shape("-1"));
        assert!(is_literal_shape("+42"));
    }

    #[test]
    fn is_literal_shape_rejects_function_calls() {
        assert!(!is_literal_shape("now()"));
        assert!(!is_literal_shape("clock_timestamp()"));
        assert!(!is_literal_shape("myapp_helper()"));
    }

    #[test]
    fn is_literal_shape_rejects_bare_identifiers() {
        assert!(!is_literal_shape("some_column"));
        assert!(!is_literal_shape("CURRENT_TIMESTAMP"));
    }
}
