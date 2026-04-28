//! Djogi build script — surfaces migration-tree drift diagnostics on
//! every `cargo build`.
//!
//! Per Phase 7 v3 §6, build.rs walks three on-disk inputs:
//!
//! 1. `target/djogi_models.json` — the descriptor inventory, written
//!    by `#[derive(Model)]` via a future macro side-channel hook.
//!    When the file does not exist (the typical state today) we treat
//!    it as an empty inventory and skip the model-vs-snapshot legs of
//!    the three-way match — only the snapshot ↔ filesystem (D004)
//!    leg is exercised.
//!
//! 2. `target/djogi_pending/<database>/<app>.json` — pending compose
//!    artifacts written by `djogi migrations compose`.
//!
//! 3. `migrations/<database>/<app>/schema_snapshot.json` — the
//!    committed schema state per bucket.
//!
//! Drift surfaces as `cargo:warning=...` lines. Suppressed entirely
//! by `Djogi.toml::build.suppress_drift_warning = true`.
//!
//! This script reads JSON only — never executes SQL, never touches
//! the database. The exact warning wording is frozen so the
//! trybuild-style expectation test pins on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    // Tell cargo to re-run if any of the three input families change.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=target/djogi_models.json");
    println!("cargo:rerun-if-changed=target/djogi_pending");
    println!("cargo:rerun-if-changed=Djogi.toml");

    // Resolve the workspace root. `CARGO_MANIFEST_DIR` points at the
    // crate root (`<workspace>/djogi`); the workspace root is its
    // parent. Tests / integrators that want to point build.rs at a
    // different root set `DJOGI_WORKSPACE_ROOT` directly.
    let workspace_root = match std::env::var_os("DJOGI_WORKSPACE_ROOT") {
        Some(root) => PathBuf::from(root),
        None => {
            let manifest_dir = match std::env::var_os("CARGO_MANIFEST_DIR") {
                Some(s) => PathBuf::from(s),
                None => return, // out-of-cargo build context; nothing to do.
            };
            manifest_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(manifest_dir)
        }
    };

    // Codex B-6: classify FIRST, then suppress only Outcome-3 drift.
    //
    // The previous code returned early on `suppress_drift_warning =
    // true`, silencing every diagnostic — including D004 filesystem
    // mismatches, Outcome 2 (composed-not-applied), and Outcome 4
    // (stale pending). Those are not the diagnostics the
    // `suppress_drift_warning` knob exists to silence; the knob's
    // purpose is to mute the noisy "model drift detected — run
    // `djogi migrations compose`" warning that fires every build
    // while a developer is actively editing schema. The other three
    // are operator-actionable signals and must always print.
    let suppress_drift = drift_warnings_suppressed(&workspace_root);
    let diagnostics = collect_diagnostics(&workspace_root);
    for d in diagnostics {
        if suppress_drift && d.is_outcome3_drift {
            continue;
        }
        println!("cargo:warning={text}", text = d.text);
    }
}

/// One classified diagnostic emitted by [`collect_diagnostics`].
///
/// The `is_outcome3_drift` flag drives Codex B-6's selective
/// suppression — only Outcome 3 (model drift) is silenced when
/// `Djogi.toml::build.suppress_drift_warning = true`.
struct BuildDiagnostic {
    text: String,
    is_outcome3_drift: bool,
}

/// Honour `Djogi.toml::build.suppress_drift_warning`.
///
/// We read the file directly with a tiny TOML-aware scanner rather
/// than pulling in a dependency: build.rs runs once per build and
/// keeping the dep tree minimal here keeps the framework's
/// compile-time footprint predictable. A missing file → not
/// suppressed (default).
///
/// No regex — byte-level scanning only.
fn drift_warnings_suppressed(workspace_root: &Path) -> bool {
    let path = workspace_root.join("Djogi.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let mut in_build_section = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // New section header. We're looking for the `[build]`
            // section; any other section turns the flag off.
            in_build_section = line == "[build]";
            continue;
        }
        if !in_build_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("suppress_drift_warning") {
            // `key = true` shape. Trim whitespace and the `=` sign.
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('=') else {
                continue;
            };
            let value = rest.trim();
            if value == "true" {
                return true;
            }
        }
    }
    false
}

/// Collect every diagnostic — Outcomes 2, 3, 4 plus the D004
/// filesystem-drift leg.
///
/// **No djogi crate import.** The build.rs runs *as a build script
/// for the djogi crate*; we cannot use `djogi::*` here because that
/// module is being compiled. We re-implement the three-way match
/// against parsed JSON only — the `migrate::build_match` module owns
/// the same logic in production code paths and the test suite pins
/// the exact warning strings, so the two implementations agree by
/// the trybuild-style expectation test.
///
/// Each returned [`BuildDiagnostic`] carries `is_outcome3_drift` so
/// the caller can suppress only Outcome 3 when
/// `Djogi.toml::build.suppress_drift_warning = true` (Codex B-6).
fn collect_diagnostics(workspace_root: &Path) -> Vec<BuildDiagnostic> {
    let mut out: Vec<BuildDiagnostic> = Vec::new();
    let migrations_root = workspace_root.join("migrations");
    let pending_root = workspace_root.join("target").join("djogi_pending");

    // Walk migrations/<database>/<app>/schema_snapshot.json files.
    let mut snapshots: BTreeMap<(String, String), JsonValue> = BTreeMap::new();
    let mut filesystem: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&migrations_root) {
        for db_entry in entries.flatten() {
            if !db_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(database) = db_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_acceptable_dir_name(database.as_bytes()) {
                continue;
            }
            let db_path = db_entry.path();
            let Ok(app_entries) = std::fs::read_dir(&db_path) else {
                continue;
            };
            for app_entry in app_entries.flatten() {
                if !app_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let Some(dirname) = app_entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if !is_acceptable_dir_name(dirname.as_bytes()) {
                    continue;
                }
                let label = if dirname == "_global_" {
                    String::new()
                } else {
                    dirname.clone()
                };
                filesystem.push((database.clone(), label.clone()));
                let snap_path = app_entry.path().join("schema_snapshot.json");
                if let Ok(text) = std::fs::read_to_string(&snap_path)
                    && let Ok(v) = parse_json(&text)
                {
                    snapshots.insert((database.clone(), label), v);
                }
            }
        }
    }

    // Walk target/djogi_pending/<database>/<app>.json. Per Codex B-7
    // we now peek `format_version` BEFORE accepting the file as a
    // valid pending plan; a future-version pending JSON surfaces a
    // version-mismatch warning instead of falling through to garbage
    // outcome classification.
    let mut pendings: BTreeMap<(String, String), JsonValue> = BTreeMap::new();
    if let Ok(db_entries) = std::fs::read_dir(&pending_root) {
        for db_entry in db_entries.flatten() {
            if !db_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(database) = db_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_acceptable_dir_name(database.as_bytes()) {
                continue;
            }
            let Ok(file_entries) = std::fs::read_dir(db_entry.path()) else {
                continue;
            };
            for f in file_entries.flatten() {
                if !f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let Some(name) = f.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let Some(stem) = name.strip_suffix(".json") else {
                    continue;
                };
                let label = if stem == "_global_" {
                    String::new()
                } else {
                    stem.to_string()
                };
                let path = f.path();
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(v) = parse_json(&text) else {
                    continue;
                };
                // Codex B-7 — pending format_version peek. The
                // [`PendingPlan`] struct uses `#[serde(deny_unknown_fields)]`
                // server-side, but build.rs reads pending JSON via a
                // raw walk; without an explicit version peek a
                // `format_version: "2"` pending file with new fields
                // would flow into `classify_outcome` and produce a
                // garbage diagnostic. We detect the mismatch and
                // emit a structured warning instead.
                if let Some(found) = peek_format_version(&v)
                    && found != PENDING_FORMAT_VERSION
                {
                    out.push(BuildDiagnostic {
                        text: format_pending_format_version_mismatch(&database, &label, found),
                        is_outcome3_drift: false,
                    });
                    // Skip this bucket's pending — we cannot trust
                    // its shape for the outcome classifier.
                    continue;
                }
                pendings.insert((database.clone(), label), v);
            }
        }
    }

    // Read the descriptor inventory side-channel. Optional — when
    // missing we skip the model-vs-snapshot legs of the match.
    let models_path = workspace_root.join("target").join("djogi_models.json");
    let models_per_bucket: BTreeMap<(String, String), JsonValue> =
        match std::fs::read_to_string(&models_path) {
            Ok(text) => match parse_json(&text) {
                Ok(JsonValue::Object(obj)) => obj
                    .into_iter()
                    .filter_map(|(key, value)| split_bucket_key(&key).map(|k| (k, value)))
                    .collect(),
                _ => BTreeMap::new(),
            },
            Err(_) => BTreeMap::new(),
        };

    // D004 — filesystem ↔ registered_apps comparisons.
    let registered_per_db = registered_apps_per_database(&snapshots);
    let fs_set: std::collections::BTreeSet<(String, String)> = filesystem.iter().cloned().collect();
    for (db, app) in &fs_set {
        let known = registered_per_db
            .get(db.as_str())
            .map(|set| set.contains(app.as_str()))
            .unwrap_or(false);
        if !known {
            out.push(BuildDiagnostic {
                text: format_d004_unregistered(db, app),
                is_outcome3_drift: false,
            });
        }
    }
    for (db, apps) in &registered_per_db {
        for app in apps {
            if !fs_set.contains(&(db.to_string(), app.to_string())) {
                out.push(BuildDiagnostic {
                    text: format_d004_missing(db, app),
                    is_outcome3_drift: false,
                });
            }
        }
    }

    // Outcome match — bucket-by-bucket.
    let mut every_bucket: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    every_bucket.extend(snapshots.keys().cloned());
    every_bucket.extend(pendings.keys().cloned());
    every_bucket.extend(models_per_bucket.keys().cloned());
    for bucket in &every_bucket {
        let m = models_per_bucket.get(bucket);
        let p = pendings.get(bucket);
        let s = snapshots.get(bucket);
        if let Some(diag) = classify_outcome(bucket, m, p, s) {
            out.push(diag);
        }
    }

    out
}

/// Pending JSON format version this Djogi understands. Mirrors
/// [`crate::migrate::compose::PENDING_FORMAT_VERSION`] — duplicated
/// here because build.rs cannot import the crate it's compiling.
const PENDING_FORMAT_VERSION: &str = "1";

/// Peek the top-level `format_version` field of a parsed JSON value.
/// Returns `None` when the value isn't an object or the field is
/// missing / non-string.
fn peek_format_version(v: &JsonValue) -> Option<&str> {
    if let JsonValue::Object(map) = v
        && let Some(JsonValue::String(s)) = map.get("format_version")
    {
        Some(s.as_str())
    } else {
        None
    }
}

fn format_pending_format_version_mismatch(database: &str, app: &str, found: &str) -> String {
    format!(
        "djogi: pending JSON format version '{found}' at {database}/{app} is not supported by this Djogi (expected '{expected}'); upgrade or check out a newer djogi",
        app = if app.is_empty() { "_global_" } else { app },
        expected = PENDING_FORMAT_VERSION,
    )
}

fn registered_apps_per_database(
    snapshots: &BTreeMap<(String, String), JsonValue>,
) -> BTreeMap<&str, std::collections::BTreeSet<&str>> {
    let mut per_db: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for ((db, _label), snap) in snapshots {
        let entry = per_db.entry(db.as_str()).or_default();
        if let JsonValue::Object(map) = snap
            && let Some(JsonValue::Array(items)) = map.get("registered_apps")
        {
            for item in items {
                if let JsonValue::String(s) = item {
                    entry.insert(s.as_str());
                }
            }
        }
    }
    per_db
}

/// Compare three JSON snapshots modulo `generated_at`. Returns the
/// frozen warning text matching the v3 §6 amendment.
fn classify_outcome(
    bucket: &(String, String),
    models: Option<&JsonValue>,
    pending_full: Option<&JsonValue>,
    snapshot: Option<&JsonValue>,
) -> Option<BuildDiagnostic> {
    // Pending JSON is the [`PendingPlan`] shape — extract the embedded
    // `model_snapshot` field for comparison purposes.
    let pending_snap = pending_full.and_then(|v| {
        if let JsonValue::Object(map) = v {
            map.get("model_snapshot")
        } else {
            None
        }
    });
    let m_eq_p = json_equiv(models, pending_snap);
    let m_eq_s = json_equiv(models, snapshot);
    let p_eq_s = json_equiv(pending_snap, snapshot);
    if pending_full.is_none() && m_eq_s {
        return None;
    }
    if pending_full.is_some() && m_eq_p && m_eq_s {
        return None;
    }
    if pending_full.is_some() && m_eq_p && !m_eq_s {
        return Some(BuildDiagnostic {
            text: format_outcome2(bucket, pending_full),
            is_outcome3_drift: false,
        });
    }
    if pending_full.is_some() && !m_eq_p && !p_eq_s {
        return Some(BuildDiagnostic {
            text: format_outcome4(bucket),
            is_outcome3_drift: false,
        });
    }
    if !m_eq_s && (pending_full.is_none() || p_eq_s) {
        return Some(BuildDiagnostic {
            text: format_outcome3(bucket),
            is_outcome3_drift: true,
        });
    }
    Some(BuildDiagnostic {
        text: format_outcome3(bucket),
        is_outcome3_drift: true,
    })
}

fn json_equiv(a: Option<&JsonValue>, b: Option<&JsonValue>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => json_equiv_inner(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn json_equiv_inner(a: &JsonValue, b: &JsonValue) -> bool {
    // Walk recursively, ignoring `generated_at` and `composed_at`
    // wherever they appear.
    match (a, b) {
        (JsonValue::Null, JsonValue::Null) => true,
        (JsonValue::Bool(x), JsonValue::Bool(y)) => x == y,
        (JsonValue::Number(x), JsonValue::Number(y)) => x == y,
        (JsonValue::String(x), JsonValue::String(y)) => x == y,
        (JsonValue::Array(xs), JsonValue::Array(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys.iter())
                    .all(|(x, y)| json_equiv_inner(x, y))
        }
        (JsonValue::Object(xs), JsonValue::Object(ys)) => {
            let xkeys: std::collections::BTreeSet<&str> = xs.keys().map(String::as_str).collect();
            let ykeys: std::collections::BTreeSet<&str> = ys.keys().map(String::as_str).collect();
            let combined: std::collections::BTreeSet<&str> = xkeys.union(&ykeys).copied().collect();
            for key in combined {
                if matches!(key, "generated_at" | "composed_at") {
                    continue;
                }
                let xv = xs.get(key);
                let yv = ys.get(key);
                if !json_equiv(xv, yv) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

// ── Frozen warning wording — must match
// `crate::migrate::build_match` byte-for-byte. The integration test
// asserts agreement.
// MIRROR: keep in lockstep with djogi::migrate::build_match

/// Codex B-8: Outcome 2 wording must include the pending migration's
/// filename + version. We dig into the parsed pending JSON to recover
/// `version`, then derive `<version>.sql` (the up-side filename per
/// `naming::up_filename`). On a malformed pending JSON we fall back
/// to placeholders so the build never panics over bad data.
fn format_outcome2(bucket: &(String, String), pending_full: Option<&JsonValue>) -> String {
    let (filename, version) = pending_full
        .and_then(|v| {
            if let JsonValue::Object(map) = v
                && let Some(JsonValue::String(version)) = map.get("version")
            {
                Some((format!("{version}.sql"), version.clone()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| ("<unknown>.sql".to_string(), "<unknown>".to_string()));
    format!(
        "composed migration not yet applied: {filename} (version {version}; bucket {database}/{app})",
        database = bucket.0,
        app = if bucket.1.is_empty() {
            "_global_"
        } else {
            bucket.1.as_str()
        },
    )
}

fn format_outcome3(bucket: &(String, String)) -> String {
    format!(
        "model drift detected for {database}/{app}; run `djogi migrations compose` to stage the delta",
        database = bucket.0,
        app = if bucket.1.is_empty() {
            "_global_"
        } else {
            bucket.1.as_str()
        },
    )
}

fn format_outcome4(bucket: &(String, String)) -> String {
    format!(
        "pending compose for {database}/{app} is stale relative to model state; re-run `djogi migrations compose`",
        database = bucket.0,
        app = if bucket.1.is_empty() {
            "_global_"
        } else {
            bucket.1.as_str()
        },
    )
}

fn format_d004_unregistered(database: &str, app: &str) -> String {
    format!(
        "D004: filesystem app \"{database}/{app}\" not registered in snapshot",
        app = if app.is_empty() { "_global_" } else { app },
    )
}

fn format_d004_missing(database: &str, app: &str) -> String {
    format!(
        "D004: registered app \"{database}/{app}\" missing from filesystem",
        app = if app.is_empty() { "_global_" } else { app },
    )
}

// ── Identifier filter ─────────────────────────────────────────────────────

fn is_acceptable_dir_name(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    // Reject dot-prefixed names (e.g. `.git`, `.github`) explicitly —
    // mirrors the same rule in `djogi::migrate::target::is_acceptable_dir_name`.
    if bytes[0] == b'.' {
        return false;
    }
    let first = bytes[0];
    if first != b'_' && !first.is_ascii_alphabetic() {
        return false;
    }
    for &b in &bytes[1..] {
        if b != b'_' && !b.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

// ── Tiny JSON parser ──────────────────────────────────────────────────────
//
// Build.rs avoids depending on `serde_json` to keep its dependency
// graph lean — the script runs once per build and a 30-line custom
// parser carries no transitive risk. The parser handles only what
// Djogi snapshot files contain: objects, arrays, strings, numbers,
// booleans, null. No comments. No trailing commas. Errors return
// `Err` so build.rs falls through to "missing" behaviour rather than
// failing the entire build over a malformed file.

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

fn parse_json(text: &str) -> Result<JsonValue, String> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let v = parse_value(bytes, &mut pos)?;
    skip_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err(format!("trailing bytes at {pos}"));
    }
    Ok(v)
}

fn skip_ws(bytes: &[u8], pos: &mut usize) {
    while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r') {
        *pos += 1;
    }
}

fn parse_value(bytes: &[u8], pos: &mut usize) -> Result<JsonValue, String> {
    skip_ws(bytes, pos);
    if *pos >= bytes.len() {
        return Err("unexpected end of input".to_string());
    }
    match bytes[*pos] {
        b'{' => parse_object(bytes, pos),
        b'[' => parse_array(bytes, pos),
        b'"' => parse_string(bytes, pos).map(JsonValue::String),
        b't' | b'f' => parse_bool(bytes, pos),
        b'n' => parse_null(bytes, pos),
        b'-' | b'0'..=b'9' => parse_number(bytes, pos),
        b => Err(format!("unexpected byte {b:#x} at {pos}", pos = *pos)),
    }
}

fn parse_object(bytes: &[u8], pos: &mut usize) -> Result<JsonValue, String> {
    *pos += 1; // consume '{'
    let mut map = BTreeMap::new();
    skip_ws(bytes, pos);
    if *pos < bytes.len() && bytes[*pos] == b'}' {
        *pos += 1;
        return Ok(JsonValue::Object(map));
    }
    loop {
        skip_ws(bytes, pos);
        let key = parse_string(bytes, pos)?;
        skip_ws(bytes, pos);
        if *pos >= bytes.len() || bytes[*pos] != b':' {
            return Err(format!("expected ':' at {pos}", pos = *pos));
        }
        *pos += 1;
        let v = parse_value(bytes, pos)?;
        map.insert(key, v);
        skip_ws(bytes, pos);
        if *pos >= bytes.len() {
            return Err("unterminated object".to_string());
        }
        match bytes[*pos] {
            b',' => *pos += 1,
            b'}' => {
                *pos += 1;
                return Ok(JsonValue::Object(map));
            }
            b => return Err(format!("expected ',' or '}}', got {b:#x}")),
        }
    }
}

fn parse_array(bytes: &[u8], pos: &mut usize) -> Result<JsonValue, String> {
    *pos += 1; // consume '['
    let mut out = Vec::new();
    skip_ws(bytes, pos);
    if *pos < bytes.len() && bytes[*pos] == b']' {
        *pos += 1;
        return Ok(JsonValue::Array(out));
    }
    loop {
        let v = parse_value(bytes, pos)?;
        out.push(v);
        skip_ws(bytes, pos);
        if *pos >= bytes.len() {
            return Err("unterminated array".to_string());
        }
        match bytes[*pos] {
            b',' => *pos += 1,
            b']' => {
                *pos += 1;
                return Ok(JsonValue::Array(out));
            }
            b => return Err(format!("expected ',' or ']', got {b:#x}")),
        }
    }
}

fn parse_string(bytes: &[u8], pos: &mut usize) -> Result<String, String> {
    if *pos >= bytes.len() || bytes[*pos] != b'"' {
        return Err(format!("expected '\"' at {pos}", pos = *pos));
    }
    *pos += 1;
    let mut out = String::new();
    while *pos < bytes.len() {
        let b = bytes[*pos];
        if b == b'"' {
            *pos += 1;
            return Ok(out);
        }
        if b == b'\\' {
            *pos += 1;
            if *pos >= bytes.len() {
                return Err("dangling escape".to_string());
            }
            match bytes[*pos] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\u{0008}'),
                b'f' => out.push('\u{000C}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    if *pos + 4 >= bytes.len() {
                        return Err("short \\u escape".to_string());
                    }
                    let hex = std::str::from_utf8(&bytes[*pos + 1..*pos + 5])
                        .map_err(|e| format!("\\u not utf8: {e}"))?;
                    let cp =
                        u32::from_str_radix(hex, 16).map_err(|e| format!("\\u not hex: {e}"))?;
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                    }
                    *pos += 4;
                }
                other => return Err(format!("unknown escape {other:#x}")),
            }
            *pos += 1;
            continue;
        }
        // Multibyte UTF-8 — copy raw bytes into the output. We rely
        // on the upstream UTF-8 validity (snapshot files come from
        // serde_json, which validates).
        out.push(b as char);
        *pos += 1;
    }
    Err("unterminated string".to_string())
}

fn parse_bool(bytes: &[u8], pos: &mut usize) -> Result<JsonValue, String> {
    if bytes[*pos..].starts_with(b"true") {
        *pos += 4;
        return Ok(JsonValue::Bool(true));
    }
    if bytes[*pos..].starts_with(b"false") {
        *pos += 5;
        return Ok(JsonValue::Bool(false));
    }
    Err("expected bool".to_string())
}

fn parse_null(bytes: &[u8], pos: &mut usize) -> Result<JsonValue, String> {
    if bytes[*pos..].starts_with(b"null") {
        *pos += 4;
        return Ok(JsonValue::Null);
    }
    Err("expected null".to_string())
}

fn parse_number(bytes: &[u8], pos: &mut usize) -> Result<JsonValue, String> {
    let start = *pos;
    if bytes[*pos] == b'-' {
        *pos += 1;
    }
    while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if *pos < bytes.len() && bytes[*pos] == b'.' {
        *pos += 1;
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            *pos += 1;
        }
    }
    if *pos < bytes.len() && (bytes[*pos] == b'e' || bytes[*pos] == b'E') {
        *pos += 1;
        if *pos < bytes.len() && (bytes[*pos] == b'+' || bytes[*pos] == b'-') {
            *pos += 1;
        }
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            *pos += 1;
        }
    }
    let raw = std::str::from_utf8(&bytes[start..*pos]).map_err(|e| e.to_string())?;
    Ok(JsonValue::Number(raw.to_string()))
}

/// `target/djogi_models.json` is keyed by `<database>/<app>` strings
/// (the macro side-channel will follow the same convention as the
/// runtime projection's `BucketKey`). We split the key into a
/// `(database, label)` pair so build.rs can match it against pending
/// and snapshot lookups directly. Returns `None` for malformed keys
/// (missing `/`, etc.) so the build.rs falls through to "skip".
fn split_bucket_key(key: &str) -> Option<(String, String)> {
    let (db, app) = key.split_once('/')?;
    let label = if app == "_global_" { "" } else { app };
    Some((db.to_string(), label.to_string()))
}
