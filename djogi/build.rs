//! Djogi build script — surfaces migration-tree drift diagnostics on
//! every `cargo build`.
//!
//! Per Phase 7 v3 §6, build.rs walks three on-disk inputs:
//!
//! 1. `target/djogi_models.json` — the descriptor inventory, written
//!    by `#[derive(Model)]` via a future macro side-channel hook.
//!    When the file does not exist (the typical state today) the
//!    model-vs-* legs are skipped silently, but the pending ↔ snapshot
//!    comparison still runs so a composed migration can surface as
//!    "not yet applied". If the file exists but is malformed, build.rs
//!    emits one loud warning naming the path/cause, then degrades to
//!    the same reduced pending ↔ snapshot path.
//!
//! 2. `target/djogi_pending/<database>/<app>.json` plus
//!    `target/djogi_pending/<database>/.phase_zero/<version>.json` —
//!    pending compose artifacts written by `djogi migrations compose`.
//!
//! 3. `migrations/<database>/<app>/schema_snapshot.json` — the
//!    committed schema state per bucket.
//!
//! Drift surfaces as `cargo:warning=...` lines. Suppressed entirely
//! by `Djogi.toml::build.suppress_drift_warning = true`.
//!
//! This script reads JSON only — never executes SQL, never touches
//! the database. The exact warning wording is frozen so the
//! `phase7_t6_build_warning_agreement` integration test can pin on
//! it byte-for-byte.

use std::collections::BTreeMap;
use std::io::ErrorKind;
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

    // Classify before suppression. `suppress_drift_warning` mutes only
    // the noisy "model drift detected — run `djogi migrations compose`"
    // warning that fires while a developer is actively editing schema.
    // Filesystem mismatches, composed-not-applied pending migrations,
    // and stale pending plans are operator-actionable and must still
    // print.
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
/// The `is_outcome3_drift` flag drives selective suppression: only
/// Outcome 3 (model drift) is silenced when
/// `Djogi.toml::build.suppress_drift_warning = true`.
pub(crate) struct BuildDiagnostic {
    pub(crate) text: String,
    pub(crate) is_outcome3_drift: bool,
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
/// the exact warning strings, so the two implementations agree via
/// the `phase7_t6_build_warning_agreement` integration test.
///
/// Each returned [`BuildDiagnostic`] carries `is_outcome3_drift` so
/// the caller can suppress only Outcome 3 when
/// `Djogi.toml::build.suppress_drift_warning = true`.
pub(crate) fn collect_diagnostics(workspace_root: &Path) -> Vec<BuildDiagnostic> {
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

    // Walk target/djogi_pending/<database>/<app>.json plus the hidden
    // Phase 0 namespace target/djogi_pending/<database>/.phase_zero/<version>.json.
    // Peek `format_version` before accepting a file as a valid pending
    // plan, so future-version pending JSON surfaces a version-mismatch
    // warning instead of falling through to garbage outcome classification.
    let mut pendings: BTreeMap<(String, String), PendingArtifacts> = BTreeMap::new();
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
                let Ok(file_type) = f.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    if f.file_name().to_str() != Some(".phase_zero") {
                        continue;
                    }
                    let Ok(phase_zero_entries) = std::fs::read_dir(f.path()) else {
                        continue;
                    };
                    for phase_zero_file in phase_zero_entries.flatten() {
                        if !phase_zero_file
                            .file_type()
                            .map(|t| t.is_file())
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        let Some(name) = phase_zero_file.file_name().to_str().map(str::to_string)
                        else {
                            continue;
                        };
                        let path = phase_zero_file.path();
                        let Ok(text) = std::fs::read_to_string(&path) else {
                            continue;
                        };
                        let Ok(v) = parse_json(&text) else {
                            continue;
                        };
                        if let Some(found) = peek_format_version(&v)
                            && found != PENDING_FORMAT_VERSION
                        {
                            out.push(BuildDiagnostic {
                                text: format_hidden_phase_zero_format_version_mismatch(
                                    &database, found,
                                ),
                                is_outcome3_drift: false,
                            });
                            continue;
                        }
                        let key =
                            match validate_hidden_phase_zero_pending_json(&v, &database, &name) {
                                Ok(key) => key,
                                Err(detail) => {
                                    out.push(BuildDiagnostic {
                                        text: format_hidden_phase_zero_authority_mismatch(
                                            &database, &detail,
                                        ),
                                        is_outcome3_drift: false,
                                    });
                                    continue;
                                }
                            };
                        pendings
                            .entry(key)
                            .or_default()
                            .insert(PendingArtifactKind::HiddenPhaseZero, v);
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Some(name) = f.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if !name.ends_with(".json") {
                    continue;
                }
                let path = f.path();
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(v) = parse_json(&text) else {
                    continue;
                };
                let path_label = name.strip_suffix(".json").unwrap_or_default();
                let path_app = if path_label == "_global_" {
                    String::new()
                } else {
                    path_label.to_string()
                };
                if let Some(found) = peek_format_version(&v)
                    && found != PENDING_FORMAT_VERSION
                {
                    out.push(BuildDiagnostic {
                        text: format_pending_format_version_mismatch(&database, &path_app, found),
                        is_outcome3_drift: false,
                    });
                    continue;
                }
                let key = match validate_normal_pending_json(&v, &database, &name) {
                    Ok(validated) => validated,
                    Err(detail) => {
                        out.push(BuildDiagnostic {
                            text: format_pending_authority_mismatch(&database, &path_app, &detail),
                            is_outcome3_drift: false,
                        });
                        continue;
                    }
                };
                pendings
                    .entry(key)
                    .or_default()
                    .insert(PendingArtifactKind::Normal, v);
            }
        }
    }

    // Read the descriptor inventory side-channel. Missing is the
    // legitimate fresh-adoption path; malformed is loud and degrades
    // to the same reduced pending↔snapshot classifier.
    let models_path = workspace_root.join("target").join("djogi_models.json");
    let models_inventory = read_models_inventory(&models_path);
    if let ModelsInventoryState::Malformed { detail } = &models_inventory {
        out.push(BuildDiagnostic {
            text: format_warning_inventory_malformed(&models_path.display().to_string(), detail),
            is_outcome3_drift: false,
        });
    }

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
    if let ModelsInventoryState::Present(models_per_bucket) = &models_inventory {
        every_bucket.extend(models_per_bucket.keys().cloned());
    }
    for bucket in &every_bucket {
        let inventory = match &models_inventory {
            ModelsInventoryState::Present(models_per_bucket) => {
                InventoryMatch::Present(models_per_bucket.get(bucket))
            }
            ModelsInventoryState::Absent | ModelsInventoryState::Malformed { .. } => {
                InventoryMatch::Absent
            }
        };
        let models = match inventory {
            InventoryMatch::Present(models) => models,
            InventoryMatch::Absent => None,
        };
        let selected_pending = pendings
            .get(bucket)
            .and_then(|artifacts| artifacts.select_for_bucket(models, snapshots.get(bucket)));
        let s = snapshots.get(bucket);
        if let Some(diag) = classify_outcome(bucket, inventory, selected_pending, s) {
            out.push(diag);
        }
    }

    out
}

/// Pending JSON format version this Djogi understands. Mirrors
/// [`crate::migrate::compose::PENDING_FORMAT_VERSION`] — duplicated
/// here because build.rs cannot import the crate it's compiling.
const PENDING_FORMAT_VERSION: &str = "1";

/// Canonical hidden Phase 0 pending version label. Mirrored from
/// `crate::migrate::bootstrap::PHASE_ZERO_VERSION` because build.rs
/// cannot import the crate it is compiling.
const PHASE_ZERO_VERSION: &str = "V00000000000000__phase_zero_bootstrap";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingArtifactKind {
    HiddenPhaseZero,
    Normal,
}

#[derive(Debug, Clone, Default)]
struct PendingArtifacts {
    hidden_phase_zero: Option<JsonValue>,
    normal: Option<JsonValue>,
}

enum ModelsInventoryState {
    Present(BTreeMap<(String, String), JsonValue>),
    Absent,
    Malformed { detail: String },
}

#[derive(Clone, Copy)]
enum InventoryMatch<'a> {
    Present(Option<&'a JsonValue>),
    Absent,
}

impl PendingArtifacts {
    fn insert(&mut self, kind: PendingArtifactKind, value: JsonValue) {
        match kind {
            PendingArtifactKind::HiddenPhaseZero => {
                self.hidden_phase_zero.get_or_insert(value);
            }
            PendingArtifactKind::Normal => {
                self.normal = Some(value);
            }
        }
    }

    fn select_for_bucket(
        &self,
        models: Option<&JsonValue>,
        _snapshot: Option<&JsonValue>,
    ) -> Option<&JsonValue> {
        if let Some(hidden) = self.hidden_phase_zero.as_ref()
            && json_equiv(models, pending_model_snapshot(hidden))
        {
            return Some(hidden);
        }
        if let Some(normal) = self.normal.as_ref()
            && json_equiv(models, pending_model_snapshot(normal))
        {
            return Some(normal);
        }
        self.hidden_phase_zero.as_ref().or(self.normal.as_ref())
    }
}

fn read_models_inventory(models_path: &Path) -> ModelsInventoryState {
    match std::fs::read_to_string(models_path) {
        Ok(text) => match parse_json(&text) {
            Ok(JsonValue::Object(obj)) => {
                let mut models_per_bucket = BTreeMap::new();
                let mut malformed_keys = Vec::new();
                for (key, value) in obj {
                    match split_bucket_key(&key) {
                        Some(bucket) => {
                            models_per_bucket.insert(bucket, value);
                        }
                        None => malformed_keys.push(key),
                    }
                }
                if malformed_keys.is_empty() {
                    ModelsInventoryState::Present(models_per_bucket)
                } else {
                    malformed_keys.sort();
                    ModelsInventoryState::Malformed {
                        detail: format!(
                            "bucket keys [{}] are not in <database>/<app> form",
                            malformed_keys.join(", ")
                        ),
                    }
                }
            }
            Ok(_) => ModelsInventoryState::Malformed {
                detail: "top-level JSON value is not an object".to_string(),
            },
            Err(err) => ModelsInventoryState::Malformed {
                detail: format!("invalid JSON: {err}"),
            },
        },
        Err(err) if err.kind() == ErrorKind::NotFound => ModelsInventoryState::Absent,
        Err(err) => ModelsInventoryState::Malformed {
            detail: format!("unreadable: {err}"),
        },
    }
}

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
    format_pending_format_version_mismatch_at(&pending_location(database, app), found)
}

fn format_hidden_phase_zero_format_version_mismatch(database: &str, found: &str) -> String {
    format_pending_format_version_mismatch_at(&hidden_phase_zero_location(database), found)
}

fn format_pending_format_version_mismatch_at(location: &str, found: &str) -> String {
    format!(
        "djogi: pending JSON format version '{found}' at {location} is not supported by this Djogi (expected '{expected}'); upgrade or check out a newer djogi",
        expected = PENDING_FORMAT_VERSION,
    )
}

fn format_pending_authority_mismatch(database: &str, app: &str, detail: &str) -> String {
    format_pending_authority_mismatch_at(&pending_location(database, app), detail)
}

fn format_hidden_phase_zero_authority_mismatch(database: &str, detail: &str) -> String {
    format_pending_authority_mismatch_at(&hidden_phase_zero_location(database), detail)
}

fn format_pending_authority_mismatch_at(location: &str, detail: &str) -> String {
    format!("djogi: pending JSON authority mismatch at {location}; {detail}",)
}

fn pending_location(database: &str, app: &str) -> String {
    format!(
        "{database}/{app}",
        app = if app.is_empty() { "_global_" } else { app },
    )
}

fn hidden_phase_zero_location(database: &str) -> String {
    format!("{database}/.phase_zero/{PHASE_ZERO_VERSION}.json")
}

fn pending_identity_fields(v: &JsonValue) -> Option<(&str, &str, &str)> {
    let JsonValue::Object(map) = v else {
        return None;
    };
    match (
        map.get("bucket_database"),
        map.get("bucket_app"),
        map.get("version"),
    ) {
        (
            Some(JsonValue::String(database)),
            Some(JsonValue::String(app)),
            Some(JsonValue::String(version)),
        ) => Some((database.as_str(), app.as_str(), version.as_str())),
        _ => None,
    }
}

fn validate_normal_pending_json(
    v: &JsonValue,
    database: &str,
    filename: &str,
) -> Result<(String, String), String> {
    let Some(stem) = filename.strip_suffix(".json") else {
        return Err("filename must end with .json".to_string());
    };
    let label = if stem == "_global_" {
        String::new()
    } else {
        if !is_acceptable_dir_name(stem.as_bytes()) {
            return Err(format!("non-canonical pending filename {filename}"));
        }
        stem.to_string()
    };
    let (payload_database, payload_app, version) =
        pending_identity_fields(v).ok_or_else(|| "missing bucket identity fields".to_string())?;
    if payload_database != database {
        return Err(format!(
            "payload database {payload_database} does not match path database {database}"
        ));
    }
    if payload_app != label {
        let expected = if label.is_empty() {
            "_global_"
        } else {
            label.as_str()
        };
        let found = if payload_app.is_empty() {
            "_global_"
        } else {
            payload_app
        };
        return Err(format!(
            "payload app {found} does not match path app {expected}"
        ));
    }
    if version == PHASE_ZERO_VERSION {
        return Err("Phase 0 pending JSON must use the hidden .phase_zero namespace".to_string());
    }
    Ok((database.to_string(), label))
}

fn validate_hidden_phase_zero_pending_json(
    v: &JsonValue,
    database: &str,
    filename: &str,
) -> Result<(String, String), String> {
    let expected_filename = format!("{PHASE_ZERO_VERSION}.json");
    if filename != expected_filename {
        return Err(format!(
            "hidden Phase 0 filename must be {expected_filename}"
        ));
    }
    let (payload_database, payload_app, version) =
        pending_identity_fields(v).ok_or_else(|| "missing bucket identity fields".to_string())?;
    if payload_database != database {
        return Err(format!(
            "payload database {payload_database} does not match path database {database}"
        ));
    }
    if !payload_app.is_empty() {
        return Err("hidden Phase 0 payload must target the global bucket".to_string());
    }
    if version != PHASE_ZERO_VERSION {
        return Err(format!(
            "hidden Phase 0 payload must use version {PHASE_ZERO_VERSION}"
        ));
    }
    Ok((database.to_string(), String::new()))
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
    inventory: InventoryMatch<'_>,
    selected_pending: Option<&JsonValue>,
    snapshot: Option<&JsonValue>,
) -> Option<BuildDiagnostic> {
    match inventory {
        InventoryMatch::Present(models) => {
            classify_outcome_present(bucket, models, selected_pending, snapshot)
        }
        InventoryMatch::Absent => classify_outcome_absent(bucket, selected_pending, snapshot),
    }
}

fn classify_outcome_absent(
    bucket: &(String, String),
    selected_pending: Option<&JsonValue>,
    snapshot: Option<&JsonValue>,
) -> Option<BuildDiagnostic> {
    let pending = selected_pending?;
    let pending_snap = pending_model_snapshot(pending);
    if json_equiv(pending_snap, snapshot) {
        return None;
    }
    Some(BuildDiagnostic {
        text: format_outcome2(bucket, selected_pending),
        is_outcome3_drift: false,
    })
}

fn classify_outcome_present(
    bucket: &(String, String),
    models: Option<&JsonValue>,
    selected_pending: Option<&JsonValue>,
    snapshot: Option<&JsonValue>,
) -> Option<BuildDiagnostic> {
    // Pending JSON is the [`PendingPlan`] shape — extract the embedded
    // `model_snapshot` field for comparison purposes.
    let pending_snap = selected_pending.and_then(pending_model_snapshot);
    let m_eq_p = json_equiv(models, pending_snap);
    let m_eq_s = json_equiv(models, snapshot);
    let p_eq_s = json_equiv(pending_snap, snapshot);
    if selected_pending.is_none() && m_eq_s {
        return None;
    }
    if selected_pending.is_some() && m_eq_p && m_eq_s {
        return None;
    }
    if selected_pending.is_some() && m_eq_p && !m_eq_s {
        return Some(BuildDiagnostic {
            text: format_outcome2(bucket, selected_pending),
            is_outcome3_drift: false,
        });
    }
    if selected_pending.is_some() && !m_eq_p && !p_eq_s {
        return Some(BuildDiagnostic {
            text: format_outcome4(bucket),
            is_outcome3_drift: false,
        });
    }
    if !m_eq_s && (selected_pending.is_none() || p_eq_s) {
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

fn pending_model_snapshot(v: &JsonValue) -> Option<&JsonValue> {
    if let JsonValue::Object(map) = v {
        map.get("model_snapshot")
    } else {
        None
    }
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

/// Outcome 2 wording includes the pending migration's filename and
/// version. We dig into the parsed pending JSON to recover `version`,
/// then derive `<version>.sdjql` (the up-side filename per
/// `naming::up_filename`). On malformed pending JSON, fall back to
/// placeholders so the build never panics over bad data.
fn format_outcome2(bucket: &(String, String), selected_pending: Option<&JsonValue>) -> String {
    let (filename, version) = selected_pending
        .and_then(|v| {
            if let JsonValue::Object(map) = v
                && let Some(JsonValue::String(version)) = map.get("version")
            {
                // Extension must match naming::MIGRATION_FILE_EXT and
                // build_match::format_warning_outcome2 byte-for-byte.
                Some((format!("{version}.sdjql"), version.clone()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| ("<unknown>.sdjql".to_string(), "<unknown>".to_string()));
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

fn format_warning_inventory_malformed(path: &str, detail: &str) -> String {
    format!(
        "descriptor inventory at {path} is malformed ({detail}); model state is treated as unavailable, so model-vs-snapshot checks are skipped for this build"
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
/// and snapshot lookups directly. Malformed keys (missing `/`, etc.)
/// are whole-file errors — callers must not silently skip them.
fn split_bucket_key(key: &str) -> Option<(String, String)> {
    let (db, app) = key.split_once('/')?;
    let label = if app == "_global_" { "" } else { app };
    Some((db.to_string(), label.to_string()))
}
