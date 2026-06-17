use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use djogi::migrate::{AppliedSchema, PENDING_FORMAT_VERSION, PendingPlan, SNAPSHOT_FORMAT_VERSION};

#[allow(dead_code)]
#[path = "../../djogi/build.rs"]
mod build_script;

/// Canonicalize a workspace path and verify it stays within a safe anchor
/// (temp directory or current working directory). Panics on containment violation.
fn safe_workspace(workspace: &Path) -> PathBuf {
    let temp = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp directory");
    let cwd = std::env::current_dir()
        .expect("current directory exists")
        .canonicalize()
        .expect("canonicalize current directory");
    let parent = workspace.parent().expect("workspace parent");
    let parent_canon = parent
        .canonicalize()
        .unwrap_or_else(|err| panic!("canonicalize workspace parent {}: {err}", parent.display()));
    let canon = parent_canon.join(workspace.file_name().expect("workspace path filename"));
    if !canon.starts_with(&temp) && !canon.starts_with(&cwd) {
        panic!(
            "workspace path {} is outside temp directory and current directory",
            canon.display()
        );
    }
    let canon = canon.canonicalize().expect("canonicalize workspace");
    if !canon.starts_with(&temp) && !canon.starts_with(&cwd) {
        panic!(
            "workspace path {} is outside temp directory and current directory",
            canon.display()
        );
    }
    canon
}

fn temp_workspace(_tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_canon = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp directory");
    let p = temp_canon.join(format!("djogi-build-pending-{nanos}-{n}"));
    if !p.starts_with(&temp_canon) {
        panic!("workspace path {} is outside temp directory", p.display());
    }
    fs::create_dir_all(&p).unwrap();
    safe_workspace(&p)
}

fn safe_write_bytes(path: &Path, bytes: impl AsRef<[u8]>) {
    let path = vetted_child_path(path);
    let parent = path.parent().expect("path parent").to_path_buf();
    let parent = safe_workspace(&parent);
    let candidate = parent.join(path.file_name().expect("path filename"));
    djogi::migrate::write_workspace_file(&parent, &candidate, bytes.as_ref()).unwrap();
}

fn vetted_child_path(path: &Path) -> PathBuf {
    let parent = path.parent().expect("path parent");
    let temp_canon = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp directory");
    let cwd_canon = std::env::current_dir()
        .expect("current directory exists")
        .canonicalize()
        .expect("canonicalize current directory");
    if !parent.starts_with(&temp_canon) && !parent.starts_with(&cwd_canon) {
        panic!(
            "path parent {} is outside temp directory and current directory",
            parent.display()
        );
    }
    let parent = safe_workspace(parent);
    parent.join(path.file_name().expect("path filename"))
}

fn schema(tag: &str) -> AppliedSchema {
    AppliedSchema {
        djogi_version: tag.to_string(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: "2026-06-06T00:00:00Z".to_string(),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: vec![String::new()],
    }
}

fn write_pending(
    path: &std::path::Path,
    database: &str,
    app: &str,
    version: &str,
    snapshot: &AppliedSchema,
) {
    write_pending_with_format_version(
        path,
        database,
        app,
        version,
        snapshot,
        PENDING_FORMAT_VERSION,
    );
}

fn write_pending_with_format_version(
    path: &std::path::Path,
    database: &str,
    app: &str,
    version: &str,
    snapshot: &AppliedSchema,
    format_version: &str,
) {
    let path = vetted_child_path(path);
    let pending = PendingPlan {
        format_version: format_version.to_string(),
        bucket_database: database.to_string(),
        bucket_app: app.to_string(),
        version: version.to_string(),
        slug: "test".to_string(),
        model_snapshot: snapshot.clone(),
        checksum_up: "V1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_string(),
        checksum_down: None,
        composed_at: "2026-06-06T00:00:00Z".to_string(),
        depends_on: Vec::new(),
    };
    if let Some(parent) = path.parent() {
        let parent = vetted_child_path(parent);
        fs::create_dir_all(&parent).unwrap();
    }
    safe_write_bytes(&path, serde_json::to_vec_pretty(&pending).unwrap());
}

fn diagnostic_texts(diagnostics: &[build_script::BuildDiagnostic]) -> Vec<&str> {
    diagnostics.iter().map(|d| d.text.as_str()).collect()
}

/// Lay down the hidden Phase 0 pending fixture without writing the
/// descriptor inventory (`target/djogi_models.json`). The
/// inventory-state tests below each set up their own inventory state
/// (present / missing / malformed), so the shared scaffolding stops
/// short of writing it.
fn write_hidden_phase_zero_pending(work: &std::path::Path, pending_schema: &AppliedSchema) {
    let work = safe_workspace(work);
    fs::create_dir_all(work.join("target")).unwrap();
    let pending_path = work
        .join("target/djogi_pending/main/.phase_zero/V00000000000000__phase_zero_bootstrap.json");
    write_pending(
        &pending_path,
        "main",
        "",
        "V00000000000000__phase_zero_bootstrap",
        pending_schema,
    );
}

/// Write a committed snapshot for the synthetic global bucket.
fn write_global_snapshot(work: &std::path::Path, snapshot_schema: &AppliedSchema) {
    let work = safe_workspace(work);
    let snapshot_path =
        vetted_child_path(&work.join("migrations/main/_global_/schema_snapshot.json"));
    fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
    safe_write_bytes(
        &snapshot_path,
        serde_json::to_vec_pretty(snapshot_schema).unwrap(),
    );
}

type InventoryWriter = fn(&std::path::Path);
type MalformedInventoryCase = (&'static str, InventoryWriter);

/// Frozen, byte-for-byte text the library emits for the hidden Phase 0
/// pending's Outcome 2 (composed not yet applied). Build.rs must agree
/// with this on the absent-inventory path.
fn library_phase_zero_outcome2() -> String {
    let bucket = djogi::migrate::projection::BucketKey {
        database: "main".into(),
        app: "".into(),
    };
    djogi::migrate::build_match::format_warning_outcome2(
        &bucket,
        Some("V00000000000000__phase_zero_bootstrap"),
    )
}

// ── Inventory-state contract (missing vs malformed vs present) ──────────────
//
// `target/djogi_models.json` has three states the build script must
// distinguish (see `djogi/build.rs` header + `docs/spec/migrations.md`
// §10.2):
//
//  - **Present** — file exists and parses to an object keyed by
//    `<database>/<app>`. The model-vs-* legs of the four-way match run.
//  - **Absent** — file legitimately missing (`NotFound`); the typical
//    state today. Model legs are skipped *silently*; pending↔snapshot
//    still classifies so a fresh hidden Phase 0 pending reports
//    Outcome 2 (composed, not yet applied) instead of Outcome 4.
//  - **Malformed** — file exists but is unreadable, not valid JSON, not
//    a top-level object, or carries a non-`<database>/<app>` key. A loud
//    warning fires, then the same reduced classification as Absent runs.

/// RED-A1 — absent inventory with a hidden Phase 0 pending must surface
/// the truthful Outcome 2 (composed not yet applied), byte-for-byte
/// equal to the library wording, and must NOT misreport Outcome 4
/// ("stale relative to model state"). This is the fresh-adoption
/// "typical state today": no `target/djogi_models.json` on disk.
#[test]
fn build_collect_diagnostics_hidden_phase_zero_without_model_inventory() {
    let work = temp_workspace("hidden_phase_zero_no_inventory");
    let pending_schema = schema("pending");
    let snapshot_schema = schema("snapshot");

    write_global_snapshot(&work, &snapshot_schema);
    write_hidden_phase_zero_pending(&work, &pending_schema);
    // NOTE: target/djogi_models.json is intentionally NOT written.

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    let expected = library_phase_zero_outcome2();
    assert!(
        texts.iter().any(|t| *t == expected),
        "absent inventory + hidden Phase 0 pending must surface Outcome 2 \
         byte-for-byte ({expected:?}); got: {texts:?}"
    );
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("stale relative to model state")),
        "absent inventory must not misreport the pending as stale (Outcome 4): {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

/// RED-A1b — the fresh-adoption variant: no inventory AND no snapshot.
/// The hidden Phase 0 pending must still report Outcome 2, never
/// Outcome 4. Traces the exact "fresh compose/apply" scenario.
#[test]
fn build_collect_diagnostics_hidden_phase_zero_without_model_inventory_or_snapshot() {
    let work = temp_workspace("hidden_phase_zero_no_inventory_no_snapshot");
    let pending_schema = schema("pending");

    write_hidden_phase_zero_pending(&work, &pending_schema);
    // No snapshot, no models — pure fresh adoption.

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|t| {
            t.contains("composed migration not yet applied") && t.contains("bucket main/_global_")
        }),
        "fresh adoption (no inventory, no snapshot) must surface Outcome 2: {texts:?}"
    );
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("stale relative to model state")),
        "fresh adoption must not misreport Outcome 4: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

/// The malformed-inventory fixtures: each writes a different broken
/// shape at the `target/djogi_models.json` path. All three must route
/// to the same loud-warn-and-degrade behaviour.
fn malformed_inventory_writers() -> Vec<MalformedInventoryCase> {
    vec![
        ("non_json", |p: &std::path::Path| {
            let p = vetted_child_path(p);
            safe_write_bytes(&p, b"not json at all");
        }),
        ("non_object_array", |p: &std::path::Path| {
            let p = vetted_child_path(p);
            safe_write_bytes(&p, b"[]");
        }),
        // A directory at the inventory path makes `read_to_string`
        // return a non-`NotFound` I/O error — the load-bearing
        // distinction from a legitimately-missing file.
        ("unreadable_directory", |p: &std::path::Path| {
            let p = vetted_child_path(p);
            fs::create_dir_all(&p).unwrap();
        }),
    ]
}

/// RED-A4 — a malformed inventory (unreadable / invalid JSON / not an
/// object) must (a) emit a loud malformed-inventory warning naming the
/// path, and (b) still degrade to the truthful Outcome 2 rather than
/// Outcome 4.
#[test]
fn build_collect_diagnostics_hidden_phase_zero_with_malformed_model_inventory() {
    for (label, writer) in malformed_inventory_writers() {
        let work = temp_workspace(&format!("hidden_phase_zero_malformed_{label}"));
        let pending_schema = schema("pending");
        let snapshot_schema = schema("snapshot");

        write_global_snapshot(&work, &snapshot_schema);
        write_hidden_phase_zero_pending(&work, &pending_schema);
        let models_path = work.join("target/djogi_models.json");
        writer(&models_path);

        let diagnostics = build_script::collect_diagnostics(&work);
        let texts = diagnostic_texts(&diagnostics);
        let path_str = models_path.display().to_string();
        assert!(
            texts.iter().any(|t| {
                t.contains("descriptor inventory")
                    && t.contains("is malformed")
                    && t.contains(&path_str)
            }),
            "[{label}] malformed inventory must emit a loud warning naming the path: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| {
                t.contains("composed migration not yet applied")
                    && t.contains("bucket main/_global_")
            }),
            "[{label}] malformed inventory must still surface Outcome 2: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("stale relative to model state")),
            "[{label}] malformed inventory must not misreport Outcome 4: {texts:?}"
        );

        let _ = fs::remove_dir_all(&work);
    }
}

/// RED-A4b — the over-fire inverse guard: a legitimately-missing
/// inventory (`NotFound`) must stay silent. No malformed warning may
/// fire on the typical fresh-adoption path.
#[test]
fn build_collect_diagnostics_missing_inventory_emits_no_malformed_warning() {
    let work = temp_workspace("missing_inventory_no_malformed");
    let pending_schema = schema("pending");
    let snapshot_schema = schema("snapshot");

    write_global_snapshot(&work, &snapshot_schema);
    write_hidden_phase_zero_pending(&work, &pending_schema);
    // No target/djogi_models.json — legitimately absent.

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        !texts.iter().any(|t| t.contains("is malformed")),
        "a missing inventory (NotFound) must stay silent — no malformed warning: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

/// RED-A6 — a parsed object carrying a non-`<database>/<app>` bucket
/// key makes the WHOLE file malformed (whole-file malformed treatment).
/// The corrupt key must not be silently skipped: the loud warning fires
/// and classification degrades to Outcome 2.
#[test]
fn build_collect_diagnostics_hidden_phase_zero_with_malformed_bucket_key_inventory() {
    let work = temp_workspace("hidden_phase_zero_malformed_key");
    let pending_schema = schema("pending");
    let snapshot_schema = schema("snapshot");

    write_global_snapshot(&work, &snapshot_schema);
    write_hidden_phase_zero_pending(&work, &pending_schema);

    // Valid JSON object, but a key that is NOT `<database>/<app>`.
    let mut models = BTreeMap::new();
    models.insert("badkey".to_string(), schema("pending"));
    let models_path = work.join("target/djogi_models.json");
    let models_path = vetted_child_path(&models_path);
    safe_write_bytes(&models_path, serde_json::to_vec_pretty(&models).unwrap());

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    let path_str = models_path.display().to_string();
    assert!(
        texts.iter().any(|t| {
            t.contains("descriptor inventory")
                && t.contains("is malformed")
                && t.contains(&path_str)
        }),
        "a non-<database>/<app> bucket key must make the whole inventory malformed: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| {
            t.contains("composed migration not yet applied") && t.contains("bucket main/_global_")
        }),
        "malformed-key inventory must degrade to Outcome 2: {texts:?}"
    );
    assert!(
        !texts
            .iter()
            .any(|t| t.contains("stale relative to model state")),
        "malformed-key inventory must not misreport Outcome 4: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_includes_hidden_phase_zero_pending() {
    let work = temp_workspace("hidden_phase_zero");
    let pending_schema = schema("pending");
    let snapshot_schema = schema("snapshot");

    let mut models = BTreeMap::new();
    models.insert("main/_global_".to_string(), pending_schema.clone());
    fs::create_dir_all(work.join("target")).unwrap();
    safe_write_bytes(
        &work.join("target/djogi_models.json"),
        serde_json::to_vec_pretty(&models).unwrap(),
    );

    let snapshot_path = work.join("migrations/main/_global_/schema_snapshot.json");
    fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
    safe_write_bytes(
        &snapshot_path,
        serde_json::to_vec_pretty(&snapshot_schema).unwrap(),
    );

    let pending_path = work
        .join("target/djogi_pending/main/.phase_zero/V00000000000000__phase_zero_bootstrap.json");
    write_pending(
        &pending_path,
        "main",
        "",
        "V00000000000000__phase_zero_bootstrap",
        &pending_schema,
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    assert!(
        diagnostics.iter().any(|diag| {
            diag.text.contains("composed migration not yet applied")
                && diag.text.contains("bucket main/_global_")
        }),
        "hidden Phase 0 pending should participate in build diagnostics: {:?}",
        diagnostics
            .iter()
            .map(|d| d.text.as_str())
            .collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_reports_pending_authority_mismatch() {
    let work = temp_workspace("authority_mismatch");
    let pending_path = work.join("target/djogi_pending/main/billing.json");
    write_pending(
        &pending_path,
        "main",
        "audit",
        "V20260606010101__mismatch",
        &schema("pending"),
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.text.contains("pending JSON authority mismatch")),
        "authority mismatch should surface as a build diagnostic: {:?}",
        diagnostics
            .iter()
            .map(|d| d.text.as_str())
            .collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_reports_hidden_phase_zero_format_version_identity() {
    let work = temp_workspace("hidden_phase_zero_format_version");
    let pending_path = work
        .join("target/djogi_pending/main/.phase_zero/V00000000000000__phase_zero_bootstrap.json");
    write_pending_with_format_version(
        &pending_path,
        "main",
        "",
        "V00000000000000__phase_zero_bootstrap",
        &schema("pending"),
        "99",
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|text| {
            text.contains("pending JSON format version '99'")
                && text.contains("at main/.phase_zero/V00000000000000__phase_zero_bootstrap.json")
        }),
        "hidden Phase 0 format mismatch must name the hidden identity: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| {
            text.contains("pending JSON format version '99'") && text.contains("main/_global_")
        }),
        "hidden Phase 0 format mismatch must not collapse to normal _global_: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_reports_hidden_phase_zero_authority_identity() {
    let work = temp_workspace("hidden_phase_zero_authority");
    let pending_path = work
        .join("target/djogi_pending/main/.phase_zero/V00000000000000__phase_zero_bootstrap.json");
    write_pending(
        &pending_path,
        "main",
        "billing",
        "V00000000000000__phase_zero_bootstrap",
        &schema("pending"),
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|text| {
            text.contains("pending JSON authority mismatch")
                && text.contains("at main/.phase_zero/V00000000000000__phase_zero_bootstrap.json")
        }),
        "hidden Phase 0 authority mismatch must name the hidden identity: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| {
            text.contains("pending JSON authority mismatch") && text.contains("main/_global_")
        }),
        "hidden Phase 0 authority mismatch must not collapse to normal _global_: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_reports_normal_global_format_version_identity() {
    let work = temp_workspace("normal_global_format_version");
    let pending_path = work.join("target/djogi_pending/main/_global_.json");
    write_pending_with_format_version(
        &pending_path,
        "main",
        "",
        "V20260606010101__normal_global",
        &schema("pending"),
        "99",
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|text| {
            text.contains("pending JSON format version '99'") && text.contains("at main/_global_")
        }),
        "normal global format mismatch must keep the normal _global_ identity: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}

/// A stale (`found < expected`) pending file must make the build
/// diagnostic tell the operator to recompose, matching the recompose
/// phrase the library's `PendingLoadError` Display produces.
/// `PENDING_FORMAT_VERSION` is `"2"`; `found = "1"` is the stale case.
#[test]
fn build_collect_diagnostics_stale_format_version_says_recompose() {
    let work = temp_workspace("stale_format_version_recompose");
    let pending_path = work.join("target/djogi_pending/main/_global_.json");
    write_pending_with_format_version(
        &pending_path,
        "main",
        "",
        "V20260606010101__normal_global",
        &schema("pending"),
        "1",
    );
    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|text| {
            text.contains("pending JSON format version '1'")
                && text.ends_with(
                    "; re-run 'djogi migrations compose' to regenerate this pending file",
                )
        }),
        "stale build diagnostic must end with '; <recompose phrase>': {texts:?}"
    );
    let _ = fs::remove_dir_all(&work);
}

/// A future (`found > expected`) pending file must make the build
/// diagnostic tell the operator to upgrade djogi, matching the upgrade
/// phrase the library's `PendingLoadError` Display produces.
/// `PENDING_FORMAT_VERSION` is `"2"`; `found = "3"` is the future case.
#[test]
fn build_collect_diagnostics_future_format_version_says_upgrade() {
    let work = temp_workspace("future_format_version_upgrade");
    let pending_path = work.join("target/djogi_pending/main/_global_.json");
    write_pending_with_format_version(
        &pending_path,
        "main",
        "",
        "V20260606010101__normal_global",
        &schema("pending"),
        "3",
    );
    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|text| {
            text.contains("pending JSON format version '3'")
                && text.ends_with(
                    "; upgrade to a newer version of djogi (or check out a newer revision)",
                )
        }),
        "future build diagnostic must end with '; <upgrade phrase>': {texts:?}"
    );
    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_rejects_normal_global_phase_zero_pending() {
    let work = temp_workspace("normal_global_phase_zero");
    let pending_path = work.join("target/djogi_pending/main/_global_.json");
    write_pending(
        &pending_path,
        "main",
        "",
        "V00000000000000__phase_zero_bootstrap",
        &schema("pending"),
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    assert!(
        diagnostics.iter().any(|diag| {
            diag.text.contains("pending JSON authority mismatch") && diag.text.contains("Phase 0")
        }),
        "normal-global Phase 0 pending must be rejected by build validation: {:?}",
        diagnostics
            .iter()
            .map(|d| d.text.as_str())
            .collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn build_collect_diagnostics_hidden_phase_zero_coexists_with_valid_normal_global() {
    let work = temp_workspace("hidden_normal_coexist");
    let hidden_schema = schema("hidden");
    let normal_schema = schema("normal");
    let snapshot_schema = schema("snapshot");

    let mut models = BTreeMap::new();
    models.insert("main/_global_".to_string(), hidden_schema.clone());
    fs::create_dir_all(work.join("target")).unwrap();
    safe_write_bytes(
        &work.join("target/djogi_models.json"),
        serde_json::to_vec_pretty(&models).unwrap(),
    );

    let snapshot_path = work.join("migrations/main/_global_/schema_snapshot.json");
    fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
    safe_write_bytes(
        &snapshot_path,
        serde_json::to_vec_pretty(&snapshot_schema).unwrap(),
    );

    write_pending(
        &work.join(
            "target/djogi_pending/main/.phase_zero/V00000000000000__phase_zero_bootstrap.json",
        ),
        "main",
        "",
        "V00000000000000__phase_zero_bootstrap",
        &hidden_schema,
    );
    write_pending(
        &work.join("target/djogi_pending/main/_global_.json"),
        "main",
        "",
        "V20260606010101__normal_global",
        &normal_schema,
    );

    let diagnostics = build_script::collect_diagnostics(&work);
    let texts = diagnostic_texts(&diagnostics);
    assert!(
        texts.iter().any(|text| {
            text.contains("composed migration not yet applied")
                && text.contains("V00000000000000__phase_zero_bootstrap.sdjql")
        }),
        "hidden Phase 0 pending must remain selectable when a valid normal global pending exists: {texts:?}"
    );
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("pending JSON authority mismatch")),
        "valid hidden and normal global pending artifacts must coexist without validation collisions: {texts:?}"
    );

    let _ = fs::remove_dir_all(&work);
}
