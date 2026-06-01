//! Crate-private SQL identifier validation.
//! This module is the single source of truth for identifier validation
//! across every djogi SQL emitter. Any macro-emitted `&'static str` that
//! eventually lands inside an `SqlAccumulator::push_sql` call — a relation
//! path's `source_column` / `target_table`, a `FieldRef`'s column name, a
//! field descriptor's `name` — goes through [`assert_plain_ident`] first so
//! broken macro emissions and hostile downstream bypass attempts produce
//! a framework-bug panic instead of malformed SQL.
//! The validator enforces the Postgres unquoted-identifier contract:
//! 1. Non-empty.
//! 2. Length ≤ 63 bytes (`NAMEDATALEN - 1`), so Rust-level and
//! Postgres-level identifier identity cannot diverge through
//! server-side truncation.
//! 3. First byte is an ASCII letter or underscore; every remaining
//! byte is ASCII alphanumeric or underscore. Djogi additionally
//! rejects the `$` byte that Postgres tolerates in unquoted
//! identifiers, to keep the class trivial to reason about.
//! (Implementation is pure `u8::is_ascii_alphabetic` /
//! `u8::is_ascii_alphanumeric` — no regex engine, no dependency.)
//! 4. Not a reserved Postgres keyword (case-insensitive; catcode `R`
//! in `pg_get_keywords` as of Postgres 18).
//! Callers that emit literals from `#[derive(Model)]` are the intended
//! audience; the panic messages read as framework bugs or bypass attempts.

/// Postgres's usable identifier length — `NAMEDATALEN - 1` on a default
/// build. Identifiers longer than this are silently truncated by the
/// server, which would let two Rust-distinct identifiers collide after
/// the first 63 bytes. Rejecting up front keeps the Rust- and SQL-level
/// identity contracts aligned.
pub(crate) const MAX_IDENT_LEN: usize = 63;

/// Framework-reserved identifier prefix.
/// Djogi reserves identifiers beginning with `__djogi_` (two leading
/// underscores + ASCII-case-insensitive `djogi` + underscore) for
/// framework-internal use — recursive CTE names (`__djogi_tree`,
/// `__djogi_closure`),
/// derived-table aliases (`__djogi_q`), synthetic column slots
/// (`__djogi_agg_N`, `__djogi_parent_id`, `__djogi_search_seq`,
/// `__djogi_edge_label`), and macro scratch identifiers. See
/// `docs/spec/reserved-identifiers.md` for the inventory and rationale.
/// User-supplied identifiers — window aliases, FTS dictionary names,
/// closure-model column accessors, outbox table names, runtime enum
/// types — must not enter this namespace, since the framework can
/// repurpose any name in it without considering it a breaking change.
/// Macro-emitted identifiers from `#[derive(Model)]` and friends *do*
/// legitimately start with `__djogi_`, so the runtime
/// [`assert_plain_ident`] / [`check_plain_ident`] validators do not
/// enforce the rule by themselves; the user-facing pair
/// [`assert_user_supplied_ident`] / [`check_user_supplied_ident`]
/// layers it on for surfaces that route adopter input into framework
/// SQL.
pub(crate) const RESERVED_DJOGI_PREFIX: &[u8] = b"__djogi_";

/// Const-stable check for the [`RESERVED_DJOGI_PREFIX`] namespace.
/// Equivalent to a case-insensitive `bytes.starts_with("__djogi_")`
/// over the alphabetic segment, but written with const-stable indexing
/// primitives so it can fire from [`const_assert_user_supplied_ident`]
/// inside `inventory::submit!` initializers.
pub(crate) const fn starts_with_reserved_djogi_prefix(bytes: &[u8]) -> bool {
    if bytes.len() < RESERVED_DJOGI_PREFIX.len() {
        return false;
    }
    let mut i = 0;
    while i < RESERVED_DJOGI_PREFIX.len() {
        let actual = if bytes[i] >= b'A' && bytes[i] <= b'Z' {
            bytes[i] | 0x20
        } else {
            bytes[i]
        };
        if actual != RESERVED_DJOGI_PREFIX[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Postgres fully-reserved keywords (catcode `R` in `pg_get_keywords`
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

/// Compile-time-evaluable identifier check.
/// Mirrors the four rules in [`assert_plain_ident`] but implemented
/// with exclusively const-stable primitives so the result can panic
/// during `const` evaluation — notably inside `inventory::submit!` on
/// the reverse-relation registry path, where the macro must produce a
/// `static` initializer.
/// `role` is the same label used by [`assert_plain_ident`] so panic
/// messages stay identical whether the check fires at const-eval or
/// runtime. Keyword lookup is a const-time linear scan of the sorted
/// [`RESERVED_KEYWORDS`] slice (const slice indexing and byte
/// comparison are stable; `slice::binary_search` is not yet const).
/// The linear scan is O(77) — a handful of microseconds per call at
/// build time and zero cost at runtime.
pub(crate) const fn const_assert_plain_ident(value: &'static str, role: &'static str) {
    // Mirror the runtime validator step-for-step. Each panic body uses
    // plain string literals (no `format_args!`) because const-panic
    // cannot format with runtime arguments — the messages drop the
    // offending value but keep the `role` hook for triage. The runtime
    // `assert_plain_ident` still emits full diagnostics with `value`
    // interpolated; both paths share the same four rules.
    let bytes = value.as_bytes();
    assert!(
        !bytes.is_empty(),
        // `role` is &'static str but can't be interpolated in const-panic;
        // a generic panic message is still actionable because the call
        // site pinpoints the emission.
        "djogi::ident: macro-emitted identifier must not be empty — this is a framework bug"
    );
    assert!(
        bytes.len() <= MAX_IDENT_LEN,
        "djogi::ident: macro-emitted identifier exceeds Postgres's 63-byte usable length \
         (NAMEDATALEN - 1) — either the proc-macro emission is broken or downstream code \
         bypassed the macro-support seal"
    );
    assert!(
        bytes[0].is_ascii_alphabetic() || bytes[0] == b'_',
        "djogi::ident: macro-emitted identifier must start with a letter or underscore \
         — either the proc-macro emission is broken or downstream code bypassed the \
         macro-support seal"
    );
    // Const-friendly indexed byte scan — `for` over a slice works in
    // const fn on current stable but requires indexing, not iteration
    // adapters like `all` / `iter.skip(1)`.
    let mut i = 1;
    while i < bytes.len() {
        let byte = bytes[i];
        assert!(
            byte.is_ascii_alphanumeric() || byte == b'_',
            "djogi::ident: macro-emitted identifier contains a non-identifier character \
             — either the proc-macro emission is broken or downstream code bypassed the \
             macro-support seal"
        );
        i += 1;
    }
    // Reserved-keyword lookup. We can't call `slice::binary_search` or
    // `str::to_ascii_lowercase` in const context, so the scan is
    // linear and case-folds byte-by-byte inline. Every byte is ASCII
    // alphanumeric or `_` by the preceding checks, so the case-fold
    // is just "toggle bit 0x20 on ASCII uppercase".
    let mut k = 0;
    while k < RESERVED_KEYWORDS.len() {
        let kw = RESERVED_KEYWORDS[k].as_bytes();
        if const_eq_ignore_ascii_case(bytes, kw) {
            // Const-panic path — `panic!` (not `assert!(false, ...)`)
            // both satisfies `clippy::assertions_on_constants` and
            // matches the "unreachable under a well-formed emission"
            // intent of the seal.
            panic!(
                "djogi::ident: macro-emitted identifier is a reserved Postgres keyword and \
                 cannot appear unquoted in generated SQL — either the proc-macro emission \
                 is broken or downstream code bypassed the macro-support seal"
            );
        }
        k += 1;
    }
    // `role` is threaded in for diagnostic parity with the runtime
    // validator; the actual string does not participate in any check.
    // Touching it keeps `#[allow(unused)]` off the signature and
    // preserves the call shape if a future const-panic feature grows
    // interpolation.
    let _ = role;
}

/// Compile-time-evaluable validator for user-supplied identifiers.
/// Adds the framework-reserved-prefix block from
/// [`check_user_supplied_ident`] to [`const_assert_plain_ident`]. Use
/// this for macro arguments that originate in adopter code and are
/// baked into SQL-facing strings or macro-expanded relation names.
/// Keep using [`const_assert_plain_ident`] for framework-generated
/// internal identifiers that legitimately live in the `__djogi_*`
/// namespace.
pub(crate) const fn const_assert_user_supplied_ident(value: &'static str, role: &'static str) {
    if starts_with_reserved_djogi_prefix(value.as_bytes()) {
        panic!(
            "djogi::ident: user-supplied identifier starts with the framework-reserved \
             `__djogi_` prefix"
        );
    }
    const_assert_plain_ident(value, role);
}

/// Const-stable case-insensitive byte-slice comparison.
/// Works on ASCII-only inputs — every caller here has already
/// passed the alphanumeric-or-underscore byte check in
/// [`const_assert_plain_ident`]. Equivalent to
/// `a.eq_ignore_ascii_case(b)` but usable in const context.
const fn const_eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        // Lowercase both bytes with the ASCII bit-flip trick. Only
        // touches 'A'..='Z' (0x41..=0x5A) and 'a'..='z' (0x61..=0x7A);
        // underscores and digits are fixed under the flip.
        let la = if a[i] >= b'A' && a[i] <= b'Z' {
            a[i] | 0x20
        } else {
            a[i]
        };
        let lb = if b[i] >= b'A' && b[i] <= b'Z' {
            b[i] | 0x20
        } else {
            b[i]
        };
        if la != lb {
            return false;
        }
        i += 1;
    }
    true
}

/// Why a single rule failed when [`check_plain_ident`] (or its
/// user-supplied twin [`check_user_supplied_ident`]) returns `Err`.
/// Each variant carries enough payload that the caller can render its
/// preferred error shape (`DjogiError`, `String`, panic message)
/// without re-walking the bytes. `Reserved` and `ReservedDjogiPrefix`
/// carry no payload because the caller already has the offending
/// name; the lookups happened against the lowercased copy / against
/// the [`RESERVED_DJOGI_PREFIX`] constant respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentError {
    /// Empty input.
    Empty,
    /// Exceeded `MAX_IDENT_LEN` bytes — payload is the offending length.
    TooLong { len: usize },
    /// First byte is not an ASCII letter or underscore.
    BadFirst { byte: u8 },
    /// A byte after the first is not ASCII alphanumeric or underscore.
    BadByte { idx: usize, byte: u8 },
    /// Lowercased name is in the [`RESERVED_KEYWORDS`] table.
    Reserved,
    /// Name starts with the framework-reserved [`RESERVED_DJOGI_PREFIX`]
    /// (`__djogi_`, ASCII-case-insensitive). Only emitted by
    /// [`check_user_supplied_ident`]; the
    /// general-purpose [`check_plain_ident`] continues to accept this
    /// prefix because macro-emitted identifiers legitimately use it.
    ReservedDjogiPrefix,
}

/// Fallible validator for runtime-supplied identifiers — the
/// `Result`-returning twin of [`assert_plain_ident`]. Returns an
/// [`IdentError`] describing which rule failed; the caller maps it
/// onto its preferred error type.
/// `check_reserved` toggles the reserved-keyword lookup. Macro-time
/// callers and SQL-emission paths set it to `true`; runtime-helper
/// callers (where wrapping in a `DO`-block makes reserved words like
/// `interval` legal) pass `false`.
/// Absorbed six near-duplicate inline validators (outbox /
/// context / fts dictionary / fts source column / jsonb path) that
/// each re-encoded these rules; this function is the single source of
/// truth.
pub(crate) fn check_plain_ident(value: &str, check_reserved: bool) -> Result<(), IdentError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(IdentError::Empty);
    }
    if bytes.len() > MAX_IDENT_LEN {
        return Err(IdentError::TooLong { len: bytes.len() });
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(IdentError::BadFirst { byte: first });
    }
    for (i, &byte) in bytes.iter().enumerate().skip(1) {
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(IdentError::BadByte { idx: i, byte });
        }
    }
    if check_reserved {
        // Stack-allocated lowercase for the reserved-keyword lookup
        // mirrors the buffer in `assert_plain_ident`.
        let mut lower_buf = [0u8; MAX_IDENT_LEN + 1];
        let len = bytes.len();
        lower_buf[..len].copy_from_slice(bytes);
        lower_buf[..len].make_ascii_lowercase();
        let lower = std::str::from_utf8(&lower_buf[..len])
            .expect("ASCII identifier must be valid UTF-8 after lowercasing");
        if RESERVED_KEYWORDS.binary_search(&lower).is_ok() {
            return Err(IdentError::Reserved);
        }
    }
    Ok(())
}

/// Validate a macro-emitted identifier against the Postgres unquoted-
/// identifier contract. Panics on the first rule violation. See the
/// module-level doc for the four rules.
/// `role` labels the identifier in the panic message (e.g.
/// `"source_column"`, `"table_name"`, `"field_name"`); it is the hook
/// an on-call engineer follows back to the failing emission site.
/// Panic (rather than `Result`) is appropriate because reaching the
/// helper with a bad identifier indicates either a broken proc-macro
/// emission or a downstream caller deliberately bypassing the macro-
/// support seal — both are framework-bug or misuse cases that the
/// caller cannot recover from.
/// Runtime twin of [`const_assert_plain_ident`]; keeping two entry
/// points (rather than having the runtime path just call the const
/// fn) lets the runtime variant emit richer `{value:?}`-interpolated
/// diagnostics that const-panic cannot today. Both paths enforce the
/// identical contract.
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
    // check above bounded `value.len` ≤ 63, so a 64-byte buffer is always
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

/// Fallible validator for user-supplied identifiers.
/// This is the user-facing twin of [`check_plain_ident`]. It enforces
/// every rule [`check_plain_ident`] enforces (non-empty, ≤ 63 bytes,
/// ASCII letter or underscore first, ASCII alphanumeric or underscore
/// after, optional reserved-keyword block) AND additionally rejects
/// names that start with the framework-reserved [`RESERVED_DJOGI_PREFIX`]
/// (`__djogi_`, ASCII-case-insensitive).
/// Use this at every surface where adopters can hand the framework an
/// SQL identifier — window-function aliases, FTS dictionary / source
/// column names, [`crate::query::closure::ClosureModel`] column
/// accessors, outbox table names, runtime enum-type names. Use the
/// plainer [`check_plain_ident`] only for derived names that the
/// framework constructs from already-validated inputs (e.g. the
/// `<table>_outbox` companion table name) where the prefix rule is
/// transitively enforced upstream.
/// `check_reserved` toggles the Postgres-keyword block in the same
/// shape as [`check_plain_ident`].
/// On failure, the prefix rejection precedes the byte-shape rejections
/// so a name like `__djogi_select` reports the more actionable
/// `ReservedDjogiPrefix` error rather than a Postgres-keyword error
/// that could mislead the caller into renaming the suffix.
pub(crate) fn check_user_supplied_ident(
    value: &str,
    check_reserved: bool,
) -> Result<(), IdentError> {
    if starts_with_reserved_djogi_prefix(value.as_bytes()) {
        return Err(IdentError::ReservedDjogiPrefix);
    }
    check_plain_ident(value, check_reserved)
}

/// Panicking validator for user-supplied identifiers carried as
/// `&'static str` — typically window-function aliases the adopter
/// passes to [`crate::expr::RowNumber::alias`] and friends.
/// Combines [`assert_plain_ident`]'s four-rule contract with the
/// framework-reserved-prefix block from
/// [`check_user_supplied_ident`]. Call this at boundaries where the
/// adopter directly hands a static identifier to the SQL emitter and
/// the failure mode is best surfaced as a panic (typed-builder API,
/// where the typed alias is known at compile-time and a typo is the
/// only realistic failure path).
/// `role` labels the identifier in the panic message — same convention
/// as [`assert_plain_ident`] — so an on-call engineer can map the
/// panic back to the offending API surface.
#[inline]
pub(crate) fn assert_user_supplied_ident(value: &'static str, role: &'static str) {
    assert!(
        !starts_with_reserved_djogi_prefix(value.as_bytes()),
        "djogi::ident: user-supplied {role} {value:?} is reserved — the `__djogi_` prefix \
         is used for framework-internal identifiers (recursive CTE names like `__djogi_tree`, \
         derived-table aliases like `__djogi_q`, aggregate-tuple slot aliases like \
         `__djogi_agg_N`). Choose a different name."
    );
    assert_plain_ident(value, role);
}

/// Debug-build-only identifier assertion. Expands to
/// [`assert_plain_ident`] under `cfg(debug_assertions)` and to nothing
/// in release builds, so the check runs in tests and `cargo run` but
/// contributes zero overhead to a production `--release` binary.
/// Intended for hot paths that accept a `&'static str` from a source
/// the compiler cannot structurally seal — notably the per-row loops
/// in `relation::prefetch` and `relation::select_related` that read
/// `descriptor.fields[].name`. Sealing the `Model` trait stops a
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

    // ── Reserved-prefix rule (issues #69, #82) ───────────────────────────────

    #[test]
    fn starts_with_reserved_djogi_prefix_basic_cases() {
        // Const helper used by both `check_user_supplied_ident` and
        // `assert_user_supplied_ident`; pin its behavior on the boundary
        // cases that matter to callers.
        assert!(starts_with_reserved_djogi_prefix(b"__djogi_"));
        assert!(starts_with_reserved_djogi_prefix(b"__djogi_q"));
        assert!(starts_with_reserved_djogi_prefix(b"__djogi_anything"));
        // Postgres folds unquoted identifiers to lowercase, so user
        // spellings with uppercase letters still resolve into the same
        // SQL namespace and must be rejected by the prefix check.
        assert!(starts_with_reserved_djogi_prefix(b"__DJOGI_q"));
        assert!(starts_with_reserved_djogi_prefix(b"__Djogi_q"));
        assert!(!starts_with_reserved_djogi_prefix(b""));
        assert!(!starts_with_reserved_djogi_prefix(b"_"));
        assert!(!starts_with_reserved_djogi_prefix(b"__djogi"));
        assert!(!starts_with_reserved_djogi_prefix(b"__DJOGI"));
        // Single-underscore prefix — used by `notify` channel names like
        // `djogi_<table>` — must NOT collide with the reservation.
        assert!(!starts_with_reserved_djogi_prefix(b"_djogi_"));
        assert!(!starts_with_reserved_djogi_prefix(b"djogi_"));
        // Different prefixes — adopters' own reserved namespaces.
        assert!(!starts_with_reserved_djogi_prefix(b"__myprefix_"));
    }

    #[test]
    fn check_plain_ident_still_accepts_djogi_prefix() {
        // Pin: the general-purpose `check_plain_ident` continues to
        // accept macro-emitted internal identifiers. Adding the
        // reservation rule here would break every framework-internal
        // emission (recursive CTE names, derived-table aliases, slot
        // aliases) — those legitimately live in the namespace.
        assert_eq!(check_plain_ident("__djogi_q", true), Ok(()));
        assert_eq!(check_plain_ident("__djogi_tree", true), Ok(()));
        assert_eq!(check_plain_ident("__djogi_agg_0", true), Ok(()));
    }

    #[test]
    fn check_user_supplied_ident_rejects_djogi_prefix() {
        // The user-facing twin must reject every shape in the reserved
        // namespace, regardless of the suffix. Pin the canonical
        // existing slot names so a later refactor cannot silently let
        // them through.
        assert_eq!(
            check_user_supplied_ident("__djogi_q", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        assert_eq!(
            check_user_supplied_ident("__djogi_tree", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        assert_eq!(
            check_user_supplied_ident("__djogi_agg_0", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        assert_eq!(
            check_user_supplied_ident("__djogi_parent_id", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        assert_eq!(
            check_user_supplied_ident("__DJOGI_q", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        assert_eq!(
            check_user_supplied_ident("__Djogi_q", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        // Surface still rejects post-prefix when applicable: `__djogi_`
        // alone (no suffix) is also reserved — the prefix is the entire
        // exclusion zone.
        assert_eq!(
            check_user_supplied_ident("__djogi_", false),
            Err(IdentError::ReservedDjogiPrefix)
        );
    }

    #[test]
    fn check_user_supplied_ident_prefix_check_precedes_byte_check() {
        // Ordering pin — `__djogi_<reserved keyword>` should report the
        // ReservedDjogiPrefix error rather than Reserved (Postgres
        // keyword), because the reservation rule trips before the byte
        // / keyword scan and gives the caller the more actionable
        // "rename your prefix" diagnostic.
        assert_eq!(
            check_user_supplied_ident("__djogi_select", true),
            Err(IdentError::ReservedDjogiPrefix)
        );
        // Likewise, an over-long name in the namespace prefers the
        // prefix error over `TooLong` because the prefix is the
        // primary failure to fix.
        let overlong = format!("__djogi_{}", "a".repeat(70));
        assert_eq!(
            check_user_supplied_ident(&overlong, false),
            Err(IdentError::ReservedDjogiPrefix)
        );
    }

    #[test]
    fn check_user_supplied_ident_accepts_non_reserved_names() {
        // Positive shapes — anything that the plain validator accepts
        // and that does not enter the reserved namespace must pass
        // unchanged.
        assert_eq!(check_user_supplied_ident("rank", true), Ok(()));
        assert_eq!(check_user_supplied_ident("dense_rank", true), Ok(()));
        assert_eq!(check_user_supplied_ident("_internal", true), Ok(()));
        // Single-underscore + djogi is NOT reserved — the rule is the
        // double-underscore prefix only. This shape mirrors the notify
        // channel names (`djogi_<table>`) the framework derives from
        // already-validated table names.
        assert_eq!(check_user_supplied_ident("djogi_orders", true), Ok(()));
        assert_eq!(check_user_supplied_ident("_djogi_q", true), Ok(()));
    }

    fn try_const_assert_user(value: &'static str) -> std::thread::Result<()> {
        std::panic::catch_unwind(|| const_assert_user_supplied_ident(value, "test_role"))
    }

    #[test]
    fn const_assert_user_supplied_ident_rejects_reserved_prefix() {
        assert!(try_const_assert_user("__djogi_q").is_err());
        assert!(try_const_assert_user("__DJOGI_q").is_err());
        assert!(try_const_assert_user("__Djogi_q").is_err());
        assert!(try_const_assert_user("plain_relation").is_ok());
        assert!(try_const_assert_user("_djogi_relation").is_ok());
    }

    #[test]
    fn check_user_supplied_ident_propagates_other_ident_errors() {
        // The user-supplied helper falls through to `check_plain_ident`
        // for non-prefix-related failures, so the underlying error
        // shape must remain visible to callers (they map it onto their
        // own diagnostic strings).
        assert_eq!(check_user_supplied_ident("", false), Err(IdentError::Empty));
        assert_eq!(
            check_user_supplied_ident("9col", false),
            Err(IdentError::BadFirst { byte: b'9' })
        );
        assert_eq!(
            check_user_supplied_ident("select", true),
            Err(IdentError::Reserved)
        );
    }

    fn try_assert_user(value: &'static str) -> std::thread::Result<()> {
        std::panic::catch_unwind(|| assert_user_supplied_ident(value, "test_role"))
    }

    #[test]
    fn assert_user_supplied_ident_panics_on_reserved_prefix() {
        // The panicking twin is the surface window-fn `.alias(...)`
        // routes through; a typo collision with `__djogi_*` must
        // produce a loud framework-bug-style panic so the adopter
        // catches it in their unit tests rather than at SQL emission.
        let caught = try_assert_user("__djogi_q");
        assert!(
            caught.is_err(),
            "assert_user_supplied_ident must panic on reserved prefix"
        );
        let caught = try_assert_user("__djogi_anything");
        assert!(caught.is_err());
    }

    #[test]
    fn assert_user_supplied_ident_accepts_valid_names() {
        assert!(try_assert_user("rank").is_ok());
        assert!(try_assert_user("dense_rank").is_ok());
        assert!(try_assert_user("_internal_alias").is_ok());
        assert!(try_assert_user("djogi_pluralized_rank").is_ok());
    }

    #[test]
    fn assert_user_supplied_ident_panics_on_other_violations() {
        // Falls through to `assert_plain_ident` for byte-shape and
        // reserved-keyword violations — the panic just routes through
        // a different message body.
        assert!(try_assert_user("").is_err());
        assert!(try_assert_user("9col").is_err());
        assert!(try_assert_user("select").is_err());
    }

    #[test]
    fn reserved_djogi_prefix_constant_is_stable() {
        // The exact byte sequence `__djogi_` is documented in
        // `docs/spec/reserved-identifiers.md` and is part of djogi's
        // public stability contract. Pin it so a refactor that, say,
        // capitalizes the bytes or swaps in a different separator
        // immediately surfaces.
        assert_eq!(RESERVED_DJOGI_PREFIX, b"__djogi_");
        assert_eq!(RESERVED_DJOGI_PREFIX.len(), 8);
    }
}
