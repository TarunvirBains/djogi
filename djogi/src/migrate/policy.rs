//! Out-of-order policy + multi-DB guardrails for the migration runner.
//!
//! # Scope (Phase 7 v3 §8 / T7)
//!
//! Two responsibilities:
//!
//! 1. **Out-of-order detection / enforcement.** A migration applies
//!    *out-of-order* when its `version` string lexically precedes some
//!    already-applied migration's version inside the same
//!    `(database, app)` bucket — practically, an operator picked up a
//!    feature-branch migration after main shipped a later one. The
//!    runner detects the conflict at apply time, sets the ledger row's
//!    `out_of_order_flag = TRUE`, and then either:
//!
//!    - **Allows with diagnostic** (local/dev default): proceeds, emits
//!      a `tracing::warn!` naming the conflicting peer.
//!    - **Rejects** (CI/prod default): refuses the apply with a typed
//!      error before any DDL runs.
//!    - **Allows with explicit override**: proceeds and records the
//!      operator-supplied reason in `partial_apply_note`.
//!
//! 2. **Localhost detection** for `attune --squash`. Squash is a hard
//!    history rewrite (deletes / coalesces local migration files +
//!    ledger rows) and is gated on `DATABASE_URL` resolving to the
//!    local machine. The localhost predicate here is the same byte-
//!    level scanner the `attune.rs` module uses.
//!
//! # No regex
//!
//! Per the Djogi-wide no-regex rule, every parser in this module is a
//! byte-level forward scan. The libpq parameter parser walks tokens
//! separated by single spaces and stops on the first `host=` / `=`
//! after an explicit `host` token. The URL parser handles
//! `postgres://[user[:pass]@]host[:port][/db]` by tracking the position
//! of the next `@`, `/`, `?`, and `:` byte indices.

use crate::config::DjogiConfig;

// ── Public types ──────────────────────────────────────────────────────────

/// Operator-facing policy for an apply that detects an out-of-order
/// migration version.
///
/// Production stability is the default lens: CI / prod environments
/// reject; development environments allow with a loud warning. The
/// explicit-override path lets an operator unblock dev iteration when
/// they have a documented reason — the reason is preserved in the
/// ledger row's `partial_apply_note` for audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutOfOrderPolicy {
    /// Allow the apply to proceed; emit a `tracing::warn!` and set
    /// `out_of_order_flag = TRUE` on the inserted ledger row.
    AllowWithDiagnostic,
    /// Reject the apply before any DDL runs; surface
    /// [`crate::migrate::RunnerError::OutOfOrderRejected`] with the
    /// conflicting peer's version + applied_at.
    Reject,
    /// Allow the apply; in addition to the diagnostic warn, persist
    /// the operator-supplied `override_reason` to the ledger row's
    /// `partial_apply_note` so the audit trail captures *why* the
    /// override was used.
    AllowExplicit {
        /// Operator-supplied rationale; non-empty by convention. The
        /// runner does not enforce non-emptiness so dev iterations
        /// can pass `String::new()`, but production callers should
        /// always set a real string.
        override_reason: String,
    },
}

impl OutOfOrderPolicy {
    /// Resolve the default policy from a [`DjogiConfig`]. Production
    /// profile and CI environments default to `Reject`; everything
    /// else defaults to `AllowWithDiagnostic`.
    ///
    /// **Detection rules:**
    ///
    /// - `config.is_production()` is the highest-precedence signal. A
    ///   `Djogi.toml` with `profile = "production"` always picks
    ///   `Reject`.
    /// - Otherwise, `CI` env var equal to `"true"` (case-insensitive
    ///   ASCII compare) selects `Reject`. CI runners universally set
    ///   `CI=true`; the case-insensitive form catches the few that
    ///   set `CI=TRUE` or `CI=True`.
    /// - Otherwise: `AllowWithDiagnostic`.
    ///
    /// The function takes a `&DjogiConfig` rather than reading the
    /// global so tests can pin a deterministic config without env
    /// var contention.
    pub fn default_for_config(config: &DjogiConfig) -> Self {
        if config.is_production() || ci_env_set() {
            OutOfOrderPolicy::Reject
        } else {
            OutOfOrderPolicy::AllowWithDiagnostic
        }
    }

    /// `true` when this policy allows the apply to proceed (with or
    /// without diagnostic / override). The runner's gate uses this to
    /// decide whether to short-circuit before inserting the pending
    /// ledger row.
    pub fn allows(&self) -> bool {
        match self {
            OutOfOrderPolicy::AllowWithDiagnostic => true,
            OutOfOrderPolicy::AllowExplicit { .. } => true,
            OutOfOrderPolicy::Reject => false,
        }
    }

    /// Operator-supplied rationale, if any. `None` for
    /// [`OutOfOrderPolicy::AllowWithDiagnostic`] and
    /// [`OutOfOrderPolicy::Reject`]; `Some(reason)` for
    /// [`OutOfOrderPolicy::AllowExplicit`].
    pub fn override_reason(&self) -> Option<&str> {
        match self {
            OutOfOrderPolicy::AllowExplicit { override_reason } => Some(override_reason.as_str()),
            _ => None,
        }
    }
}

/// Returns `true` when the `CI` env var is set to a value that ASCII-
/// matches `"true"` (case-insensitive). Used by
/// [`OutOfOrderPolicy::default_for_config`] to flip the default policy
/// to `Reject` on CI runners.
///
/// Implementation note: explicit ASCII comparison rather than
/// `to_lowercase` so we never allocate. `b'T'.eq_ignore_ascii_case(&b't')`
/// is the per-byte primitive.
fn ci_env_set() -> bool {
    match std::env::var("CI") {
        Ok(v) => ascii_eq_ignore_case(v.as_bytes(), b"true"),
        Err(_) => false,
    }
}

/// Byte-level ASCII case-insensitive equality. Both inputs must be
/// ASCII; non-ASCII bytes compare verbatim. No allocation.
fn ascii_eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if !x.eq_ignore_ascii_case(y) {
            return false;
        }
    }
    true
}

// ── Localhost detection (used by attune --squash) ─────────────────────────

/// Allowlist of hostnames that count as "localhost" for the purposes
/// of `attune --squash`'s safety gate. Sorted for `binary_search`.
///
/// **The empty string is intentionally listed.** A libpq connection
/// string with no `host=` parameter (or a URL with no host component)
/// defaults to a Unix-domain socket against the local machine — which
/// is local for our purposes.
const LOCALHOST_ALLOWLIST: &[&str] = &["", "127.0.0.1", "::1", "localhost"];

/// Returns `true` when the supplied connection string resolves to the
/// local machine. Recognises both forms:
///
/// - libpq parameter form: `host=localhost user=foo dbname=bar`
/// - URL form: `postgres://[user[:pass]@]host[:port][/db]` (and the
///   `postgresql://` alias)
///
/// The host extraction is byte-level — explicit forward scans, no
/// regex. Comparisons against [`LOCALHOST_ALLOWLIST`] use binary
/// search.
///
/// **Used by `attune --squash`.** The squash path refuses to run when
/// this returns `false`, so a misconfigured DATABASE_URL pointing at a
/// shared dev server cannot accidentally rewrite history that other
/// developers also pull from.
pub fn is_localhost_connection(conn: &str) -> bool {
    let host = extract_host(conn);
    LOCALHOST_ALLOWLIST.binary_search(&host).is_ok()
}

/// Pull the host component out of a libpq parameter string or URL.
/// Returns the literal byte slice (as `&str`) or `""` when none is
/// present (which the allowlist treats as localhost since it implies
/// the libpq default Unix-socket connection).
fn extract_host(conn: &str) -> &str {
    let trimmed = conn.trim();
    if trimmed.is_empty() {
        return "";
    }
    // URL form: `postgres://...` or `postgresql://...`. Recognise the
    // scheme prefix without allocating.
    if let Some(rest) = strip_scheme(trimmed) {
        return extract_url_host(rest);
    }
    // Otherwise treat as libpq parameter form.
    extract_libpq_host(trimmed)
}

/// Strip the `postgres://` or `postgresql://` scheme if present.
/// Returns the byte slice past the `://`; `None` when no scheme.
fn strip_scheme(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("postgres://") {
        return Some(rest);
    }
    if let Some(rest) = s.strip_prefix("postgresql://") {
        return Some(rest);
    }
    None
}

/// Extract the host from a URL body — `[user[:pass]@]host[:port][/db]`.
/// Walks the bytes once: find the rightmost `@` before the first `/`
/// or `?` (those terminate the authority), then split the remaining
/// authority on `:` to peel off the port.
fn extract_url_host(body: &str) -> &str {
    let bytes = body.as_bytes();
    // Find the end of the authority (first `/` or `?`).
    let mut authority_end = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'/' || b == b'?' {
            authority_end = i;
            break;
        }
    }
    let authority = &body[..authority_end];
    // Find the rightmost `@` in the authority — anything before it is
    // the user-info, anything after it is `host[:port]`.
    let host_port = match authority.rfind('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };
    // Bracketed IPv6 form: `[::1]:5432`. The closing `]` terminates
    // the host even though the address contains `:`.
    if let Some(rest) = host_port.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &rest[..end];
    }
    // Malformed bracketed form (`[` with no matching `]`) falls
    // through to the plain split below — the result is still safe: a
    // host that contains `[` will not match the allowlist and squash
    // will refuse to run.
    // Plain `host[:port]` — split on the first `:`.
    match host_port.find(':') {
        Some(idx) => &host_port[..idx],
        None => host_port,
    }
}

/// Extract the host from a libpq parameter string —
/// `key=value key=value …` separated by spaces. Returns the value of
/// the *last* `host=` key (libpq's documented "last wins" semantics).
///
/// Quoting (`'`-delimited values with backslash escapes) is supported
/// per the libpq grammar: a value may start with `'` and run until
/// the next unescaped `'`. Outside the quoted form, the value runs
/// until the next ASCII whitespace byte.
///
/// Empty input → empty host (the allowlist treats that as localhost
/// since libpq defaults to a Unix-domain socket).
fn extract_libpq_host(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut last_host_start: Option<usize> = None;
    let mut last_host_end: usize = 0;

    let mut i = 0usize;
    while i < bytes.len() {
        // Skip leading whitespace before each token.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Read the key up to '='.
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_end = i;
        // If we ran out of input, or hit whitespace without an `=`,
        // skip to the next token.
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1; // consume '='
        // Read the value. Quoted form starts with `'`.
        if i < bytes.len() && bytes[i] == b'\'' {
            i += 1; // consume opening quote
            let inner_start = i;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\'' {
                    break;
                }
                i += 1;
            }
            let inner_end = i;
            // Consume the closing quote when present.
            if i < bytes.len() && bytes[i] == b'\'' {
                i += 1;
            }
            if matches_key(&bytes[key_start..key_end], b"host") {
                last_host_start = Some(inner_start);
                last_host_end = inner_end;
            }
            continue;
        }
        // Unquoted form: value runs until the next whitespace byte.
        let value_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value_end = i;
        if matches_key(&bytes[key_start..key_end], b"host") {
            last_host_start = Some(value_start);
            last_host_end = value_end;
        }
    }

    match last_host_start {
        Some(start) => &s[start..last_host_end],
        None => "",
    }
}

/// Byte-equality check for a libpq parameter key. Keys are
/// case-sensitive in libpq; we compare verbatim.
fn matches_key(key: &[u8], target: &[u8]) -> bool {
    key == target
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DjogiConfig;

    /// Construct a [`DjogiConfig`] with a specific profile field —
    /// shared helper for the policy default tests.
    fn cfg_with_profile(profile: &str) -> DjogiConfig {
        DjogiConfig {
            profile: profile.to_string(),
            ..DjogiConfig::default()
        }
    }

    // ── OutOfOrderPolicy::default_for_config ─────────────────────────────

    #[test]
    fn default_for_config_dev_profile_allows() {
        // Belt-and-braces: clear CI so the test passes regardless of
        // the host's CI env var. tests run with --test-threads=1 per
        // the project's pre-commit policy so concurrent env mutation
        // is not a concern.
        let prior = std::env::var("CI").ok();
        // SAFETY: serial test execution; no other thread reads CI.
        unsafe {
            std::env::remove_var("CI");
        }
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::AllowWithDiagnostic);
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CI", v);
            }
        }
    }

    #[test]
    fn default_for_config_production_profile_rejects() {
        let prior = std::env::var("CI").ok();
        unsafe {
            std::env::remove_var("CI");
        }
        let cfg = cfg_with_profile("production");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::Reject);
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CI", v);
            }
        }
    }

    #[test]
    fn default_for_config_ci_env_rejects_even_in_dev() {
        let prior = std::env::var("CI").ok();
        // SAFETY: serial test execution; no other thread reads CI.
        unsafe {
            std::env::set_var("CI", "true");
        }
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::Reject);
        match prior {
            Some(v) => unsafe { std::env::set_var("CI", v) },
            None => unsafe { std::env::remove_var("CI") },
        }
    }

    #[test]
    fn default_for_config_ci_uppercase_also_rejects() {
        let prior = std::env::var("CI").ok();
        unsafe {
            std::env::set_var("CI", "TRUE");
        }
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::Reject);
        match prior {
            Some(v) => unsafe { std::env::set_var("CI", v) },
            None => unsafe { std::env::remove_var("CI") },
        }
    }

    #[test]
    fn default_for_config_ci_arbitrary_string_does_not_reject() {
        // Some CI runners use `CI=1` instead of `CI=true`. Our policy
        // is intentionally narrow — we only flip on the literal
        // `"true"` (case-insensitive) value. `CI=1` falls through to
        // the dev default.
        //
        // The narrow form is the safer default because it puts the
        // burden of opting-in on the operator: an unfamiliar value
        // never silently produces production-grade rejection. Setting
        // `CI=true` is the canonical convention.
        let prior = std::env::var("CI").ok();
        unsafe {
            std::env::set_var("CI", "1");
        }
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::AllowWithDiagnostic);
        match prior {
            Some(v) => unsafe { std::env::set_var("CI", v) },
            None => unsafe { std::env::remove_var("CI") },
        }
    }

    // ── allows / override_reason accessors ───────────────────────────────

    #[test]
    fn allows_returns_true_for_allow_variants() {
        assert!(OutOfOrderPolicy::AllowWithDiagnostic.allows());
        assert!(
            OutOfOrderPolicy::AllowExplicit {
                override_reason: "cherry-pick from main".to_string(),
            }
            .allows()
        );
    }

    #[test]
    fn allows_returns_false_for_reject() {
        assert!(!OutOfOrderPolicy::Reject.allows());
    }

    #[test]
    fn override_reason_returned_only_for_allow_explicit() {
        assert_eq!(
            OutOfOrderPolicy::AllowWithDiagnostic.override_reason(),
            None
        );
        assert_eq!(OutOfOrderPolicy::Reject.override_reason(), None);
        let p = OutOfOrderPolicy::AllowExplicit {
            override_reason: "documented reason".to_string(),
        };
        assert_eq!(p.override_reason(), Some("documented reason"));
    }

    // ── ascii_eq_ignore_case ─────────────────────────────────────────────

    #[test]
    fn ascii_eq_ignore_case_basic() {
        assert!(ascii_eq_ignore_case(b"true", b"true"));
        assert!(ascii_eq_ignore_case(b"True", b"true"));
        assert!(ascii_eq_ignore_case(b"TRUE", b"true"));
        assert!(!ascii_eq_ignore_case(b"truth", b"true"));
        assert!(!ascii_eq_ignore_case(b"", b"true"));
        assert!(ascii_eq_ignore_case(b"", b""));
    }

    // ── extract_host: URL form ────────────────────────────────────────────

    #[test]
    fn extract_host_url_simple() {
        assert_eq!(extract_host("postgres://localhost/db"), "localhost");
        assert_eq!(extract_host("postgres://localhost:5432/db"), "localhost");
    }

    #[test]
    fn extract_host_url_with_userinfo() {
        assert_eq!(
            extract_host("postgres://user:pass@localhost:5432/db"),
            "localhost"
        );
        assert_eq!(extract_host("postgres://user@localhost/db"), "localhost");
    }

    #[test]
    fn extract_host_url_postgresql_alias() {
        assert_eq!(extract_host("postgresql://localhost/db"), "localhost");
    }

    #[test]
    fn extract_host_url_no_path() {
        assert_eq!(extract_host("postgres://localhost"), "localhost");
        assert_eq!(extract_host("postgres://localhost:5432"), "localhost");
    }

    #[test]
    fn extract_host_url_127_0_0_1() {
        assert_eq!(extract_host("postgres://127.0.0.1:5432/db"), "127.0.0.1");
    }

    #[test]
    fn extract_host_url_remote_host() {
        assert_eq!(
            extract_host("postgres://db.prod.example.com:5432/main"),
            "db.prod.example.com"
        );
    }

    #[test]
    fn extract_host_url_ipv6_bracketed() {
        // IPv6 in URL form must be bracketed per RFC 3986.
        assert_eq!(extract_host("postgres://[::1]:5432/db"), "::1");
        assert_eq!(extract_host("postgres://user@[::1]:5432/db"), "::1");
        assert_eq!(extract_host("postgres://[::1]/db"), "::1");
    }

    #[test]
    fn extract_host_url_with_query_params() {
        // Query params are part of the path component for our purposes;
        // the authority ends at the first `?`.
        assert_eq!(
            extract_host("postgres://localhost?sslmode=disable"),
            "localhost"
        );
    }

    // ── extract_host: libpq parameter form ────────────────────────────────

    #[test]
    fn extract_host_libpq_basic() {
        assert_eq!(extract_host("host=localhost dbname=test"), "localhost");
    }

    #[test]
    fn extract_host_libpq_no_host_param() {
        assert_eq!(extract_host("dbname=test user=postgres"), "");
    }

    #[test]
    fn extract_host_libpq_with_quotes() {
        assert_eq!(extract_host("host='localhost' dbname=test"), "localhost");
    }

    #[test]
    fn extract_host_libpq_last_wins() {
        // libpq documents that when a key appears multiple times, the
        // last occurrence wins. Mirror that.
        assert_eq!(
            extract_host("host=remote.example.com host=127.0.0.1"),
            "127.0.0.1"
        );
    }

    #[test]
    fn extract_host_libpq_empty_string() {
        assert_eq!(extract_host(""), "");
    }

    #[test]
    fn extract_host_libpq_remote() {
        assert_eq!(
            extract_host("host=db.prod.example.com port=5432"),
            "db.prod.example.com"
        );
    }

    // ── is_localhost_connection ──────────────────────────────────────────

    #[test]
    fn is_localhost_connection_url_localhost() {
        assert!(is_localhost_connection("postgres://localhost/test"));
        assert!(is_localhost_connection("postgres://localhost:5432/test"));
        assert!(is_localhost_connection(
            "postgres://user:pass@localhost:5432/test"
        ));
    }

    #[test]
    fn is_localhost_connection_url_127_0_0_1() {
        assert!(is_localhost_connection("postgres://127.0.0.1:5432/test"));
        assert!(is_localhost_connection("postgresql://127.0.0.1/test"));
    }

    #[test]
    fn is_localhost_connection_url_ipv6() {
        assert!(is_localhost_connection("postgres://[::1]:5432/test"));
    }

    #[test]
    fn is_localhost_connection_url_remote_rejected() {
        assert!(!is_localhost_connection(
            "postgres://db.prod.example.com:5432/main"
        ));
        assert!(!is_localhost_connection("postgres://10.0.0.5/test"));
        // A near-miss: `localhostt` is a different hostname.
        assert!(!is_localhost_connection("postgres://localhostt/test"));
    }

    #[test]
    fn is_localhost_connection_libpq_localhost() {
        assert!(is_localhost_connection("host=localhost dbname=test"));
        assert!(is_localhost_connection("host=127.0.0.1 dbname=test"));
        assert!(is_localhost_connection("host=::1 dbname=test"));
    }

    #[test]
    fn is_localhost_connection_libpq_no_host_param() {
        // No host= parameter ⇒ libpq default is a Unix-domain socket
        // on the local machine ⇒ localhost for our purposes.
        assert!(is_localhost_connection("dbname=test"));
        assert!(is_localhost_connection(""));
        assert!(is_localhost_connection("   "));
    }

    #[test]
    fn is_localhost_connection_libpq_remote_rejected() {
        assert!(!is_localhost_connection(
            "host=db.prod.example.com dbname=test"
        ));
        assert!(!is_localhost_connection("host=10.0.0.5"));
    }

    #[test]
    fn is_localhost_connection_libpq_quoted_localhost() {
        assert!(is_localhost_connection("host='localhost' dbname=test"));
    }
}
