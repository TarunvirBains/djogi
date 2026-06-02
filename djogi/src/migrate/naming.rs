//! Canonical version slug + filename naming for composed migrations.
//! Two responsibilities:
//! 1. **Version IDs.** A composed migration is named
//!    `V<YYYYMMDDHHMMSS>__<sanitized-slug>` (e.g.
//!    `V20260425010203__add_users`). The `V`-prefixed timestamp is the
//!    sortable version key; the slug is the operator-facing name. Two
//!    composes against the same descriptor inventory + snapshot
//!    produce identical SQL because the timestamp is taken from the
//!    caller (compose-time clock, not differ-time clock) and the rest
//!    of the lowering is deterministic — passing the same instant
//!    through this module twice yields the same version ID.
//! 2. **Filename slugs.** The on-disk migration file is
//!    `<version>__<slug>.sdjql` (up side) and `<version>__<slug>.down.sdjql`
//!    (down side). The slug is sanitised down to a strict identifier
//!    grammar so tooling — file globbing, `git diff`, commit messages
//!    never has to worry about whitespace, punctuation, or non-ASCII
//!    bytes inside a migration filename.
//! # Slug grammar (no regex)
//! Per the project-wide no-regex rule, the sanitiser walks the input
//! byte-by-byte and applies these rules:
//! - Each input byte is lowercased if it's `b'A'..=b'Z'`.
//! - ASCII alphanumerics (`b'0'..=b'9'`, `b'a'..=b'z'`) pass through
//!   unchanged.
//! - Whitespace bytes (`b' '`, `b'\t'`, `b'-'`) collapse to a single
//!   `b'_'`.
//! - Underscores pass through.
//! - Every other byte (punctuation, multi-byte UTF-8) is dropped.
//! - Repeated underscores collapse to one.
//! - Leading and trailing underscores are trimmed.
//! - The first byte must be `b'_'` or `u8::is_ascii_alphabetic`. If
//!   the first surviving byte is a digit, the slug is prefixed with
//!   `m_` so the resulting identifier is a valid Postgres-style name.
//! - The total length is capped at 63 bytes (matching the Postgres
//!   identifier byte limit). Longer slugs are truncated; the
//!   timestamp prefix lives in a separate field so the slug truncation
//!   does not collide with the version key.
//!   Rules are spelled out in plain English here and implemented with
//!   byte-level checks below — no regex notation.
//! # Determinism
//! The sanitiser is a pure function of its byte input. The version-ID
//! constructor is a pure function of an `OffsetDateTime` (caller
//! supplies the instant). Two callers passing the same input produce
//! the same output, on every platform, with no environmental
//! dependencies.

use time::OffsetDateTime;

/// Maximum byte length of the sanitised slug (Postgres identifier
/// limit). Operators occasionally pass long migration descriptions;
/// we keep the prefix-plus-slug fits-in-one-filename invariant.
pub const MAX_SLUG_LEN: usize = 63;

/// File extension for composed migration SQL files.
/// `.sdjql` — a framework-specific extension that discourages rote manual
/// execution via `psql`. AI agents pattern-match on `.sql`; using a custom
/// extension breaks that chain. The scanner (`scan_filesystem_with_files`)
/// rejects legacy schema migration `.sql` artifacts with a diagnostic; seed SQL
/// files remain `.sql` and are handled by the seed subsystem.
pub const MIGRATION_FILE_EXT: &str = ".sdjql";

/// Byte length of [`MIGRATION_FILE_EXT`] including the leading dot.
/// Used by stem extraction to avoid magic numbers.
pub const MIGRATION_FILE_EXT_LEN: usize = MIGRATION_FILE_EXT.len();

/// Suffix for the down-side extension (`.down.sdjql`).
pub(crate) const MIGRATION_DOWN_SUFFIX: &str = ".down.sdjql";

/// Prefix used to make a slug identifier-safe when its first byte
/// would otherwise be a digit. Two letters so the result still fits
/// `MAX_SLUG_LEN`.
const DIGIT_LEAD_PREFIX: &str = "m_";

/// Sanitise an operator-supplied migration name into a strict slug.
/// Empty input — and input that sanitises to an empty byte sequence
/// returns the literal `migration` so the caller still receives a
/// well-formed filename component. Operators who want a more specific
/// slug pass `--name`; the empty-fallback exists so a forgotten
/// `--name` flag doesn't produce a malformed filename.
pub fn sanitize_slug(input: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(input.len().min(MAX_SLUG_LEN));
    let mut last_was_underscore = true; // skip leading underscores
    for byte in input.bytes() {
        let mapped: Option<u8> = match byte {
            // ASCII uppercase → lowercase.
            b'A'..=b'Z' => Some(byte + 32),
            // Lowercase ASCII letters — passthrough.
            b'a'..=b'z' => Some(byte),
            // ASCII digits — passthrough.
            b'0'..=b'9' => Some(byte),
            // Underscore — passthrough.
            b'_' => Some(b'_'),
            // Whitespace / dash — collapse to underscore.
            b' ' | b'\t' | b'\n' | b'\r' | b'-' | b'.' => Some(b'_'),
            // Everything else (punctuation, high bytes from multi-
            // byte UTF-8) — drop.
            _ => None,
        };
        let Some(b) = mapped else {
            continue;
        };
        if b == b'_' {
            if last_was_underscore {
                continue;
            }
            last_was_underscore = true;
            out.push(b);
        } else {
            last_was_underscore = false;
            out.push(b);
        }
        // Cap the working length here to avoid runaway buffers on
        // pathological multi-megabyte inputs. We cap at MAX_SLUG_LEN
        // *plus* one to leave room for the `m_` prefix below; the
        // final truncation happens after that check.
        if out.len() >= MAX_SLUG_LEN + DIGIT_LEAD_PREFIX.len() {
            break;
        }
    }
    // Trim trailing underscore.
    while out.last() == Some(&b'_') {
        out.pop();
    }
    if out.is_empty() {
        return "migration".to_string();
    }
    // First byte rule — prefix with `m_` if leading byte is a digit.
    if let Some(&first) = out.first()
        && first.is_ascii_digit()
    {
        let mut prefixed: Vec<u8> = Vec::with_capacity(out.len() + DIGIT_LEAD_PREFIX.len());
        prefixed.extend_from_slice(DIGIT_LEAD_PREFIX.as_bytes());
        prefixed.extend_from_slice(&out);
        out = prefixed;
    }
    // Length cap — final truncation.
    if out.len() > MAX_SLUG_LEN {
        out.truncate(MAX_SLUG_LEN);
        // Truncation may have left a trailing underscore; trim again.
        while out.last() == Some(&b'_') {
            out.pop();
        }
    }
    // Safe: every surviving byte is ASCII per the byte rules above.
    String::from_utf8(out).expect("sanitiser only emits ASCII")
}

/// Construct a `V<YYYYMMDDHHMMSS>` timestamp prefix from an instant.
/// `instant` is taken as a parameter so callers can pin a deterministic
/// value (tests) or pass `OffsetDateTime::now_utc()` (production
/// compose). The output is exactly 15 ASCII bytes (`V` plus 14 digits).
pub fn version_prefix(instant: OffsetDateTime) -> String {
    let utc = instant.to_offset(time::UtcOffset::UTC);
    // Manual zero-padded format — keeps us off `format_description`
    // round-trips and guarantees byte-stable output.
    let mut s = String::with_capacity(15);
    s.push('V');
    push_pad4(&mut s, utc.year() as u32);
    push_pad2(&mut s, utc.month() as u8 as u32);
    push_pad2(&mut s, utc.day() as u32);
    push_pad2(&mut s, utc.hour() as u32);
    push_pad2(&mut s, utc.minute() as u32);
    push_pad2(&mut s, utc.second() as u32);
    s
}

/// Combine a version prefix and a slug into the canonical version ID
/// (e.g. `V20260425010203__add_users`).
pub fn version_id(prefix: &str, slug: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + 2 + slug.len());
    s.push_str(prefix);
    s.push_str("__");
    s.push_str(slug);
    s
}

/// Filename for the up-side migration — `<version>.sdjql`.
/// The version is already `V<ts>__<slug>`; we deliberately keep the up
/// file name flat (no `.up.sdjql`) so the most common artifact reads
/// like a normal migration file. The down side gets the explicit suffix.
pub fn up_filename(version: &str) -> String {
    format!("{version}{ext}", ext = MIGRATION_FILE_EXT)
}

/// Filename for the down-side migration — `<version>.down.sdjql`.
pub fn down_filename(version: &str) -> String {
    format!("{version}{suffix}", suffix = MIGRATION_DOWN_SUFFIX)
}

/// Filename for the per-app pending JSON — `<app>.json`.
/// The pending JSON is keyed by app, not by version, because there
/// can be at most one pending compose per `(database, app)` at a time.
/// Re-running compose with the same `--name` overwrites this file
/// atomically (per).
pub fn pending_json_filename(app_label: &str) -> String {
    if app_label.is_empty() {
        // Synthetic global bucket — use a stable token instead of an
        // empty filename so file-system tooling does not choke.
        "_global_.json".to_string()
    } else {
        format!("{app_label}.json")
    }
}

fn push_pad4(s: &mut String, n: u32) {
    let _ = std::fmt::Write::write_fmt(s, format_args!("{n:04}"));
}

fn push_pad2(s: &mut String, n: u32) {
    let _ = std::fmt::Write::write_fmt(s, format_args!("{n:02}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replacement for `time::macros::datetime!` (feature-gated and
    /// not enabled in djogi's `time` dep). Builds a UTC instant from
    /// explicit components.
    fn at(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        let date = time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day)
            .unwrap();
        let time = time::Time::from_hms(hour, minute, second).unwrap();
        date.with_time(time).assume_utc()
    }

    #[test]
    fn empty_input_produces_migration_fallback() {
        assert_eq!(sanitize_slug(""), "migration");
        assert_eq!(sanitize_slug("   "), "migration");
        assert_eq!(sanitize_slug("___"), "migration");
        assert_eq!(sanitize_slug("!!!"), "migration");
    }

    #[test]
    fn ascii_passthrough() {
        assert_eq!(sanitize_slug("add_users"), "add_users");
        assert_eq!(sanitize_slug("AddUsers"), "addusers");
    }

    #[test]
    fn whitespace_collapses_to_single_underscore() {
        assert_eq!(sanitize_slug("add users"), "add_users");
        assert_eq!(sanitize_slug("add   users"), "add_users");
        assert_eq!(sanitize_slug("add\tusers"), "add_users");
        assert_eq!(sanitize_slug("add-users"), "add_users");
    }

    #[test]
    fn punctuation_dropped() {
        assert_eq!(sanitize_slug("add!@#$%^&*users"), "addusers");
        assert_eq!(sanitize_slug("add(users)"), "addusers");
    }

    #[test]
    fn multibyte_utf8_dropped() {
        // Each non-ASCII byte (high bit set) is dropped; ASCII bytes
        // pass the byte-rule filter unchanged. Multi-underscore runs
        // collapse to one. The exact ASCII-letter survivors depend on
        // which bytes the input string actually carries — we rely on
        // the well-known UTF-8 spelling of the Polish input below.
        assert_eq!(sanitize_slug("add_zażółć_gęślą_jaźń"), "add_za_gl_ja");
        // All non-ASCII input → migration fallback.
        assert_eq!(sanitize_slug("Привет"), "migration");
    }

    #[test]
    fn leading_digits_prefixed_with_m() {
        assert_eq!(sanitize_slug("123_things"), "m_123_things");
    }

    #[test]
    fn leading_trailing_underscore_trimmed() {
        assert_eq!(sanitize_slug("___add_users___"), "add_users");
    }

    #[test]
    fn length_cap_at_63_bytes() {
        // 80 characters of `a` becomes 63.
        let long = "a".repeat(80);
        let s = sanitize_slug(&long);
        assert_eq!(s.len(), MAX_SLUG_LEN);
        assert!(s.bytes().all(|b| b == b'a'));
    }

    #[test]
    fn length_cap_does_not_leave_trailing_underscore() {
        // A long input whose 63rd byte is `_` should trim to 62.
        let mut input = String::new();
        for _ in 0..62 {
            input.push('a');
        }
        input.push_str("____");
        let s = sanitize_slug(&input);
        // Final byte must not be underscore.
        assert!(!s.ends_with('_'), "got {s:?}");
        assert_eq!(s.len(), 62);
    }

    #[test]
    fn version_prefix_byte_stable() {
        let when = at(2026, 4, 25, 1, 2, 3);
        assert_eq!(version_prefix(when), "V20260425010203");
    }

    #[test]
    fn version_prefix_pads_single_digit_components() {
        let when = at(2026, 1, 2, 3, 4, 5);
        assert_eq!(version_prefix(when), "V20260102030405");
    }

    #[test]
    fn version_prefix_lowest_minute() {
        let when = at(2026, 12, 31, 23, 59, 59);
        assert_eq!(version_prefix(when), "V20261231235959");
    }

    #[test]
    fn version_id_concatenates_with_double_underscore() {
        let v = version_id("V20260425010203", "add_users");
        assert_eq!(v, "V20260425010203__add_users");
    }

    #[test]
    fn up_and_down_filenames() {
        let v = "V20260425010203__add_users";
        assert_eq!(up_filename(v), "V20260425010203__add_users.sdjql");
        assert_eq!(down_filename(v), "V20260425010203__add_users.down.sdjql",);
    }

    #[test]
    fn migration_file_ext_constant_has_expected_value() {
        assert_eq!(MIGRATION_FILE_EXT, ".sdjql");
        assert_eq!(MIGRATION_FILE_EXT_LEN, 6); // includes dot
    }

    #[test]
    fn up_filename_uses_sdjql_extension() {
        let v = "V20260425010203__add_users";
        assert_eq!(up_filename(v), "V20260425010203__add_users.sdjql");
    }

    #[test]
    fn down_filename_uses_down_sdjql_extension() {
        let v = "V20260425010203__add_users";
        assert_eq!(down_filename(v), "V20260425010203__add_users.down.sdjql");
    }

    #[test]
    fn down_side_contains_dot_down_marker() {
        let v = "V20260425010203__add_users";
        let down = down_filename(v);
        assert!(
            down.contains(".down."),
            "down filename must contain .down. marker: {down}"
        );
    }

    #[test]
    fn pending_json_global_bucket_uses_stable_token() {
        assert_eq!(pending_json_filename(""), "_global_.json");
        assert_eq!(pending_json_filename("billing"), "billing.json");
    }

    #[test]
    fn sanitize_byte_stable_on_repeat() {
        // Determinism test — two calls produce identical bytes.
        let a = sanitize_slug("Add some VERY long migration name -- with junk!!!!");
        let b = sanitize_slug("Add some VERY long migration name -- with junk!!!!");
        assert_eq!(a, b);
    }
}
