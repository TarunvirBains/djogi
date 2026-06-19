//! Out-of-order policy + multi-DB guardrails for the migration runner.
//! # Scope
//! Two responsibilities:
//! 1. **Out-of-order detection / enforcement.** A migration applies
//!    *out-of-order* when its `version` string lexically precedes some
//!    already-applied migration's version inside the same
//!    `(database, app)` bucket — practically, an operator picked up a
//!    feature-branch migration after main shipped a later one. The
//!    runner detects the conflict at apply time, sets the ledger row's
//!    `out_of_order_flag = TRUE`, and then either:
//! - **Allows with diagnostic** (local/dev default): proceeds, emits
//!   a `tracing::warn!` naming the conflicting peer.
//! - **Rejects** (CI/prod default): refuses the apply with a typed
//!   error before any DDL runs.
//! - **Allows with explicit override**: proceeds and records the
//!   operator-supplied reason in `partial_apply_note`.
//! 2. **Localhost detection** for `attune --squash`. Squash is a hard
//!    history rewrite (deletes / coalesces local migration files +
//!    ledger rows) and is gated on `DATABASE_URL` resolving to the
//!    local machine. The localhost predicate here is the same byte-
//!    level scanner the `attune.rs` module uses.
//! # No regex
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
    /// **Detection rules:**
    /// - `config.is_production()` is the highest-precedence signal. A
    ///   `Djogi.toml` with `profile = "production"` always picks
    ///   `Reject`.
    /// - Otherwise, `CI` env var equal to `"true"` (case-insensitive
    ///   ASCII compare) selects `Reject`. CI runners universally set
    ///   `CI=true`; the case-insensitive form catches the few that
    ///   set `CI=TRUE` or `CI=True`.
    /// - Otherwise: `AllowWithDiagnostic`.
    ///   The function takes a `&DjogiConfig` rather than reading the
    ///   global so tests can pin a deterministic config without env
    ///   var contention.
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
/// Promoted to `pub(crate)` so sibling modules (e.g.
/// `attune::djogi_env_is_production`) can reuse the primitive without
/// duplicating the loop.
pub(crate) fn ascii_eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
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
/// **The empty string is intentionally listed.** A libpq connection
/// string with no `host=` parameter (or a URL with no host component)
/// defaults to a Unix-domain socket against the local machine — which
/// is local for our purposes.
const LOCALHOST_ALLOWLIST: &[&str] = &["", "127.0.0.1", "::1", "localhost"];

/// Returns `true` when the supplied connection string resolves to the
/// local machine. Recognises both forms:
/// - libpq parameter form: `host=localhost user=foo dbname=bar`
/// - URL form: `postgres://[user[:pass]@]host[:port][/db]` (and the
///   `postgresql://` alias)
///   The host extraction is byte-level — explicit forward scans, no
///   regex. Comparisons against [`LOCALHOST_ALLOWLIST`] use binary
///   search; addresses in the IPv4 `127.0.0.0/8` loopback range (e.g.
///   `127.5.10.20`) match via the byte-level [`is_ipv4_loopback_range`]
///   helper that walks the four octets without parsing into a numeric
///   type.
///   **Used by `attune --squash`, `db reset`, and `db seed`.** The
///   squash path refuses to run when this returns `false`, so a
///   misconfigured DATABASE_URL pointing at a shared dev server cannot
///   accidentally rewrite history that other developers also pull
///   from.
pub fn is_localhost_connection(conn: &str) -> bool {
    let host = extract_host(conn);
    if LOCALHOST_ALLOWLIST.binary_search(&host).is_ok() {
        return true;
    }
    // entire `127.0.0.0/8` range so an operator running a Postgres on
    // `127.5.10.20` (a perfectly valid loopback address per RFC 5735)
    // is recognised as localhost. Allowlist is sorted + binary-searched
    // for the canonical names; the loopback-range walk handles the
    // numeric IPv4 case without parsing into a numeric type.
    is_ipv4_loopback_range(host)
}

/// dotted-quad whose first octet is `127`. The remaining three
/// octets must each be one to three ASCII decimal digits in the 0..=255
/// range; anything else (non-digit byte, octet out of range, wrong
/// number of dots) returns `false`.
/// **No regex.** The walk is a four-octet forward scan — split on `.`,
/// confirm each segment is decimal, parse via accumulator, range-check.
/// `127.0.0.1` is in [`LOCALHOST_ALLOWLIST`] (the binary-search path
/// catches it first); this helper is for the broader `127.x.y.z` shape.
fn is_ipv4_loopback_range(host: &str) -> bool {
    let bytes = host.as_bytes();
    let mut octets = [0u16; 4];
    let mut octet_idx = 0usize;
    let mut acc: u16 = 0;
    let mut digits_in_octet: u8 = 0;
    for &b in bytes {
        if b == b'.' {
            if digits_in_octet == 0 || octet_idx >= 3 {
                return false;
            }
            octets[octet_idx] = acc;
            octet_idx += 1;
            acc = 0;
            digits_in_octet = 0;
            continue;
        }
        if !b.is_ascii_digit() {
            return false;
        }
        if digits_in_octet >= 3 {
            return false;
        }
        acc = acc * 10 + (b - b'0') as u16;
        if acc > 255 {
            return false;
        }
        digits_in_octet += 1;
    }
    // Closing octet — must be present and non-empty.
    if octet_idx != 3 || digits_in_octet == 0 {
        return false;
    }
    octets[3] = acc;
    octets[0] == 127
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

/// Extract the host from a libpq parameter string
/// `key=value key=value …` separated by ASCII whitespace. Returns the
/// value of the *last* `host=` key (libpq's documented "last wins"
/// semantics).
/// **Whitespace tolerance.** Per libpq's documented connection-string
/// grammar, a keyword/value pair may have ASCII whitespace surrounding
/// the `=` separator: `host = prod`, `host = prod`, `host= prod`,
/// and `host =prod` all assign value `prod` to key `host`. The
/// previous parser only accepted the no-space form `host=prod` and
/// silently produced an empty host for any other shape — that empty
/// host then collated to localhost via the allowlist, which is exactly
/// the bug closed: a remote DATABASE_URL with whitespace-padded
/// `=` falsely passed the localhost gate.
/// Quoting is supported in BOTH the single-quoted form (a value
/// surrounded by ASCII apostrophe bytes) and the double-quoted form
/// (a value surrounded by ASCII double-quote bytes) per the libpq
/// grammar — a value may start with `'` or `"` and run until the next
/// unescaped matching quote byte, with `\` escaping the following
/// byte. Outside a quoted form, the value runs until the next ASCII
/// whitespace byte. Round-2 A-2 added the double-quoted variant; the
/// single-quoted path was wired up by .
/// Empty input → empty host (the allowlist treats that as localhost
/// since libpq defaults to a Unix-domain socket).
/// **Empty-host edge case (.** A pathological
/// input like `host= dbname=test` follows libpq's actual grammar:
/// libpq skips whitespace after `=` and then reads the value up to the
/// next whitespace byte, which means the next token (`dbname=test`)
/// becomes the value of `host`. Our parser mirrors that behaviour
/// verbatim. The result is a non-localhost host string for ambiguous
/// input, which is the safe-bias direction for the localhost gate:
/// the gate refuses, and the squash refuses to run rather than
/// guessing localhost. We leave this behaviour untouched on purpose
/// changing it would diverge from libpq and would loosen the gate.
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
        // Read the key — up to the first whitespace byte or `=`.
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_end = i;
        // Skip whitespace BETWEEN the key and the `=` (libpq tolerates
        // `host = prod` and `host = prod`).
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // If the next byte is not `=`, this token had no value — skip
        // it and continue scanning.
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1; // consume '='
        // Skip whitespace AFTER the `=` (libpq tolerates `host= prod`
        // and `host = prod`).
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // Read the value. Quoted form starts with `'` (single) or `"`
        // (double). libpq accepts both variants with identical
        // backslash-escape semantics; we mirror that.
        if i < bytes.len() && (bytes[i] == b'\'' || bytes[i] == b'"') {
            let quote = bytes[i];
            i += 1; // consume opening quote
            let inner_start = i;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    break;
                }
                i += 1;
            }
            let inner_end = i;
            // Consume the closing quote when present.
            if i < bytes.len() && bytes[i] == quote {
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

    struct EnvGuard {
        key: &'static str,
        value: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, new_value: Option<&str>) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                match new_value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { key, value }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.value {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Construct a [`DjogiConfig`] with a specific profile field
    /// shared helper for the policy default tests.
    fn cfg_with_profile(profile: &str) -> DjogiConfig {
        DjogiConfig {
            profile: profile.to_string(),
            ..DjogiConfig::default()
        }
    }

    // ── OutOfOrderPolicy::default_for_config ─────────────────────────────

    #[serial_test::serial]
    #[test]
    fn default_for_config_dev_profile_allows() {
        // Belt-and-braces: clear CI so the test passes regardless of
        // the host's CI env var. `#[serial_test::serial]` (default key)
        // gives this test exclusive process-wide env access, so
        // concurrent env mutation by another test is not a concern.
        let _guard = EnvGuard::set("CI", None);
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::AllowWithDiagnostic);
    }

    #[serial_test::serial]
    #[test]
    fn default_for_config_production_profile_rejects() {
        let _guard = EnvGuard::set("CI", None);
        let cfg = cfg_with_profile("production");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::Reject);
    }

    #[serial_test::serial]
    #[test]
    fn default_for_config_ci_env_rejects_even_in_dev() {
        let _guard = EnvGuard::set("CI", Some("true"));
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::Reject);
    }

    #[serial_test::serial]
    #[test]
    fn default_for_config_ci_uppercase_also_rejects() {
        let _guard = EnvGuard::set("CI", Some("TRUE"));
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::Reject);
    }

    #[serial_test::serial]
    #[test]
    fn default_for_config_ci_arbitrary_string_does_not_reject() {
        // Some CI runners use `CI=1` instead of `CI=true`. Our policy
        // is intentionally narrow — we only flip on the literal
        // `"true"` (case-insensitive) value. `CI=1` falls through to
        // the dev default.
        // The narrow form is the safer default because it puts the
        // burden of opting-in on the operator: an unfamiliar value
        // never silently produces production-grade rejection. Setting
        // `CI=true` is the canonical convention.
        let _guard = EnvGuard::set("CI", Some("1"));
        let cfg = cfg_with_profile("development");
        let policy = OutOfOrderPolicy::default_for_config(&cfg);
        assert_eq!(policy, OutOfOrderPolicy::AllowWithDiagnostic);
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

    // ── regression: whitespace-padded `=` in libpq form ──────────────

    /// Padded `host = prod` must extract `prod`, not the empty string.
    /// The empty-string case previously short-circuited through the
    /// allowlist as localhost — which falsely passed the squash gate
    /// against a remote database.
    #[test]
    fn extract_host_libpq_padded_equals_single_space_each_side() {
        assert_eq!(extract_host("host = prod dbname=test"), "prod");
    }

    #[test]
    fn extract_host_libpq_padded_equals_double_space_each_side() {
        assert_eq!(extract_host("host  =  prod dbname=test"), "prod");
    }

    #[test]
    fn extract_host_libpq_padded_equals_only_after() {
        assert_eq!(extract_host("host=  prod dbname=test"), "prod");
    }

    #[test]
    fn extract_host_libpq_padded_equals_only_before() {
        assert_eq!(extract_host("host  =prod dbname=test"), "prod");
    }

    #[test]
    fn extract_host_libpq_padded_equals_quoted_value() {
        assert_eq!(
            extract_host("host = 'prod with space' dbname=test"),
            "prod with space"
        );
    }

    #[test]
    fn extract_host_libpq_padded_equals_remote_hostname() {
        // The full trigger: `host = prod.example.com` previously
        // returned `""` and `is_localhost_connection` treated `""` as
        // localhost (Unix-socket convention). Verify the parser now
        // returns the full hostname so the squash gate refuses.
        assert_eq!(
            extract_host("host = prod.example.com dbname=main"),
            "prod.example.com"
        );
        assert!(!is_localhost_connection(
            "host = prod.example.com dbname=main"
        ));
    }

    #[test]
    fn is_localhost_connection_libpq_padded_equals_remote_rejected() {
        // Same as above but exercising the public predicate directly.
        assert!(!is_localhost_connection(
            "host = db.prod.example.com dbname=test"
        ));
        assert!(!is_localhost_connection("host  =  10.0.0.5 dbname=test"));
        assert!(!is_localhost_connection("host=  prod dbname=test"));
        assert!(!is_localhost_connection("host  =prod dbname=test"));
    }

    #[test]
    fn is_localhost_connection_libpq_padded_equals_localhost_still_passes() {
        assert!(is_localhost_connection("host = localhost dbname=test"));
        assert!(is_localhost_connection("host  =  127.0.0.1 dbname=test"));
        assert!(is_localhost_connection("host=  ::1 dbname=test"));
    }

    // ── Round-2 double-quoted libpq values ──────────────────────────

    /// `host="hostname"` must extract `hostname` — without the double
    /// quotes, exactly as the single-quoted form does. The pre-A-2
    /// parser saw the leading `"` as a non-quote byte and produced the
    /// quoted-with-quotes string, which never matched the localhost
    /// allowlist.
    #[test]
    fn extract_host_libpq_double_quoted_value() {
        assert_eq!(extract_host("host=\"localhost\" dbname=test"), "localhost");
    }

    /// Double-quoted values may contain whitespace just like the
    /// single-quoted form.
    #[test]
    fn extract_host_libpq_double_quoted_with_space() {
        assert_eq!(
            extract_host("host=\"prod with space\" dbname=test"),
            "prod with space"
        );
    }

    /// A value opened with `"` is closed by `"`, not `'` (and vice
    /// versa). A mixed-quote token like `host="x'y"` retains the inner
    /// `'` literally; a token like `host='x"y'` retains the inner `"`.
    #[test]
    fn extract_host_libpq_mixed_quotes() {
        // Opening `"` is closed by `"` — the `'` inside is literal.
        assert_eq!(extract_host("host=\"x'y\" dbname=test"), "x'y");
        // Opening `'` is closed by `'` — the `"` inside is literal.
        assert_eq!(extract_host("host='x\"y' dbname=test"), "x\"y");
    }

    /// `is_localhost_connection` must recognise double-quoted localhost
    /// the same way it recognises the single-quoted form (covered
    /// the single-quoted path; A-2 closes the double-quoted gap).
    #[test]
    fn is_localhost_connection_libpq_double_quoted_localhost() {
        assert!(is_localhost_connection("host=\"localhost\" dbname=test"));
        assert!(is_localhost_connection("host=\"127.0.0.1\" dbname=test"));
        assert!(!is_localhost_connection(
            "host=\"db.prod.example.com\" dbname=test"
        ));
    }

    /// Round-3 A-2 closeout: backslash escape inside a quoted value
    /// does NOT terminate the quoted region. The parser tracks each
    /// backslash plus the next byte as a 2-byte unit, so a `\"`
    /// inside `"..."` keeps the value open through the inner `"`.
    /// Important: because `extract_libpq_host` returns a `&str` slice
    /// of the original input, the captured value preserves the raw
    /// bytes including the backslash escape. It does NOT unescape
    /// (that would require allocation). For the localhost gate this
    /// is safe: a hostname containing `\` cannot match the allowlist
    /// (`localhost`, `127.0.0.1`, `::1`), so the gate fails closed.
    /// If a future use case needs the unescaped form, change the
    /// signature to `Cow<'_, str>` and unescape only when needed.
    #[test]
    fn extract_host_libpq_double_quoted_with_escaped_quote() {
        // `host="foo\"bar"` — the inner `\"` is consumed as a 2-byte
        // unit, keeping the quoted region open. The captured slice
        // is the raw `foo\"bar` (including backslash) per the doc
        // above.
        assert_eq!(
            extract_host("host=\"foo\\\"bar\" dbname=test"),
            "foo\\\"bar"
        );
        // Mirror form: single-quoted value with escaped `'`.
        assert_eq!(extract_host("host='foo\\'bar' dbname=test"), "foo\\'bar");
        // The localhost gate correctly fails closed — neither raw
        // string is in the allowlist.
        assert!(!is_localhost_connection("host=\"foo\\\"bar\" dbname=test"));
        assert!(!is_localhost_connection("host='foo\\'bar' dbname=test"));
    }

    /// Round-3 A-2 closeout: the `host= dbname=test` empty-value edge
    /// case. Per the libpq grammar documented at the parser, libpq
    /// itself skips whitespace after `=` and reads the next non-
    /// whitespace token as the value — so `host= dbname=test` parses
    /// as `host = "dbname=test"`. Our parser mirrors that. The
    /// localhost gate then rejects `dbname=test` (not in the allowlist),
    /// which is the safe-bias direction: ambiguous connection strings
    /// fail closed (refuse to assume localhost) rather than fail open.
    #[test]
    fn extract_host_libpq_empty_value_consumes_next_token() {
        // The current behaviour mirrors libpq: the value runs up to
        // the next whitespace, so `dbname=test` is captured as the
        // host literal.
        assert_eq!(extract_host("host= dbname=test"), "dbname=test");
        // The localhost predicate then refuses this — `dbname=test`
        // is not in the allowlist, so the gate fails closed.
        assert!(!is_localhost_connection("host= dbname=test"));
    }

    // ── 0.0.0/8 IPv4 loopback range ──────────

    /// Every host in the IPv4 loopback range (`127.0.0.0/8` per
    /// RFC 5735) must be recognised as localhost. The allowlist
    /// already carries `127.0.0.1`; the helper extends the recognition
    /// to the entire range without parsing into a numeric type.
    #[test]
    fn u_partial_is_ipv4_loopback_range_accepts_127_dot_x_y_z() {
        assert!(is_ipv4_loopback_range("127.0.0.1"));
        assert!(is_ipv4_loopback_range("127.0.0.0"));
        assert!(is_ipv4_loopback_range("127.5.10.20"));
        assert!(is_ipv4_loopback_range("127.255.255.254"));
        assert!(is_ipv4_loopback_range("127.255.255.255"));
        assert!(is_ipv4_loopback_range("127.1.1.1"));
    }

    /// Non-127 IPv4 addresses must NOT match the helper.
    #[test]
    fn u_partial_is_ipv4_loopback_range_rejects_non_127_addresses() {
        assert!(!is_ipv4_loopback_range("128.0.0.1"));
        assert!(!is_ipv4_loopback_range("10.0.0.1"));
        assert!(!is_ipv4_loopback_range("192.168.1.1"));
        assert!(!is_ipv4_loopback_range("0.0.0.0"));
        assert!(!is_ipv4_loopback_range("126.255.255.255"));
        assert!(!is_ipv4_loopback_range("255.255.255.255"));
    }

    /// Malformed inputs must NOT match (defence-in-depth — a host
    /// string that does not parse as an IPv4 dotted-quad falls through
    /// to a closed gate).
    #[test]
    fn u_partial_is_ipv4_loopback_range_rejects_malformed_inputs() {
        assert!(!is_ipv4_loopback_range(""));
        assert!(!is_ipv4_loopback_range("127"));
        assert!(!is_ipv4_loopback_range("127.0"));
        assert!(!is_ipv4_loopback_range("127.0.0"));
        assert!(!is_ipv4_loopback_range("127.0.0.1.5")); // 5 octets
        assert!(!is_ipv4_loopback_range("127.0.0."));
        assert!(!is_ipv4_loopback_range(".127.0.0.1"));
        assert!(!is_ipv4_loopback_range("127..0.1"));
        assert!(!is_ipv4_loopback_range("127.0.0.256")); // octet out of range
        assert!(!is_ipv4_loopback_range("127.0.0.999"));
        assert!(!is_ipv4_loopback_range("127.a.0.1")); // non-digit
        assert!(!is_ipv4_loopback_range("127.0.0.0001")); // 4 digits in an octet
        assert!(!is_ipv4_loopback_range("localhost")); // not a dotted-quad
        // `[::1]` looks loopback but is IPv6 — recognised separately
        // via the `LOCALHOST_ALLOWLIST` exact match path.
        assert!(!is_ipv4_loopback_range("::1"));
    }

    /// `is_localhost_connection` integrates the new helper so URL
    /// and libpq forms with a `127.x.y.z` host both pass the gate.
    #[test]
    fn u_partial_is_localhost_connection_recognises_full_127_range() {
        assert!(is_localhost_connection("postgres://127.5.10.20:5432/test"));
        assert!(is_localhost_connection("postgres://127.0.42.1/test"));
        assert!(is_localhost_connection("postgres://127.255.255.254/test"));
        assert!(is_localhost_connection("host=127.5.10.20 dbname=test"));
        assert!(is_localhost_connection("host='127.0.42.1' dbname=test"));
        // Non-127 address must still refuse.
        assert!(!is_localhost_connection("postgres://10.0.0.5/test"));
        assert!(!is_localhost_connection("host=128.0.0.1 dbname=test"));
    }
}
