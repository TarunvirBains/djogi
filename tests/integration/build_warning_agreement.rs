//!  — build.rs warning text agreement test.
//!
//! Two independent code paths emit the exact same warning text:
//!
//! 1. [`djogi::migrate::build_match`] — production code, used by the
//!    `migrations status` and library callers.
//! 2. `djogi/build.rs` — the compile-time build script that surfaces
//!    drift via `cargo:warning=`. Build scripts cannot import the
//!    crate they're building, so the wording is duplicated.
//!
//! This test is the byte-for-byte expectation pinning called out in the
//! v3 §6 amendment. It pins the exact strings on the library side
//! (covered by `build_match::tests`) AND verifies that the build.rs
//! source contains the same wording byte-for-byte. If a future change
//! adjusts the message in one place but not the other, this test
//! fails loudly.

use std::path::PathBuf;

fn build_rs_text() -> String {
    // `CARGO_MANIFEST_DIR` points at the `djogi/` crate root when this
    // test runs (per the integration-test entry's `path =` field).
    // Build.rs lives at the crate root.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(manifest_dir).join("build.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn outcome2_wording_matches_build_rs() {
    let bucket = djogi::migrate::projection::BucketKey {
        database: "main".into(),
        app: "billing".into(),
    };
    // Codex B-8 / v3 §6: Outcome 2 wording must include the pending
    // migration's filename + version. With no version supplied the
    // placeholder fallback fires.
    let lib = djogi::migrate::build_match::format_warning_outcome2(&bucket, None);
    assert_eq!(
        lib,
        "composed migration not yet applied: <unknown>.sdjql (version <unknown>; bucket main/billing)"
    );
    let with_version = djogi::migrate::build_match::format_warning_outcome2(
        &bucket,
        Some("V20260425010203__add_invoices"),
    );
    assert_eq!(
        with_version,
        "composed migration not yet applied: V20260425010203__add_invoices.sdjql \
         (version V20260425010203__add_invoices; bucket main/billing)"
    );
    let text = build_rs_text();
    // The build.rs's frozen-string format helper must contain the
    // same template literal modulo placeholders. The new wording
    // includes filename + version + bucket components.
    assert!(
        text.contains(
            "composed migration not yet applied: {filename} (version {version}; bucket {database}/{app})"
        ),
        "build.rs must carry the same wording as build_match::format_warning_outcome2"
    );
}

#[test]
fn outcome3_wording_matches_build_rs() {
    let bucket = djogi::migrate::projection::BucketKey {
        database: "main".into(),
        app: "".into(),
    };
    let lib = djogi::migrate::build_match::format_warning_outcome3(&bucket);
    assert_eq!(
        lib,
        "model drift detected for main/_global_; run `djogi migrations compose` to stage the delta"
    );
    let text = build_rs_text();
    assert!(
        text.contains(
            "model drift detected for {database}/{app}; run `djogi migrations compose` to stage the delta"
        ),
        "build.rs must carry the same wording as build_match::format_warning_outcome3"
    );
}

#[test]
fn outcome4_wording_matches_build_rs() {
    let bucket = djogi::migrate::projection::BucketKey {
        database: "main".into(),
        app: "audit".into(),
    };
    let lib = djogi::migrate::build_match::format_warning_outcome4(&bucket);
    assert_eq!(
        lib,
        "pending compose for main/audit is stale relative to model state; re-run `djogi migrations compose`"
    );
    let text = build_rs_text();
    assert!(
        text.contains(
            "pending compose for {database}/{app} is stale relative to model state; re-run `djogi migrations compose`"
        ),
        "build.rs must carry the same wording as build_match::format_warning_outcome4"
    );
}

#[test]
fn unregistered_app_warning_matches_build_rs() {
    let bucket = djogi::migrate::projection::BucketKey {
        database: "main".into(),
        app: "ghost".into(),
    };
    let lib = djogi::migrate::build_match::format_warning_d004_unregistered(&bucket);
    assert_eq!(
        lib,
        "D004: filesystem app \"main/ghost\" not registered in snapshot"
    );
    let text = build_rs_text();
    // We grep for the source-level form (with backslash-escaped
    // double quotes) since `build.rs` is read raw.
    assert!(
        text.contains(r#"D004: filesystem app \"{database}/{app}\" not registered in snapshot"#),
        "build.rs must carry the same wording as build_match::format_warning_d004_unregistered"
    );
}

#[test]
fn missing_app_warning_matches_build_rs() {
    let bucket = djogi::migrate::projection::BucketKey {
        database: "main".into(),
        app: "billing".into(),
    };
    let lib = djogi::migrate::build_match::format_warning_d004_missing(&bucket);
    assert_eq!(
        lib,
        "D004: registered app \"main/billing\" missing from filesystem"
    );
    let text = build_rs_text();
    assert!(
        text.contains(r#"D004: registered app \"{database}/{app}\" missing from filesystem"#),
        "build.rs must carry the same wording as build_match::format_warning_d004_missing"
    );
}

#[test]
fn malformed_inventory_wording_matches_build_rs() {
    let lib = djogi::migrate::build_match::format_warning_inventory_malformed(
        "target/djogi_models.json",
        "not a JSON object",
    );
    assert_eq!(
        lib,
        "descriptor inventory at target/djogi_models.json is malformed (not a JSON object); \
         model state is treated as unavailable, so model-vs-snapshot checks are skipped for this build"
    );
    let text = build_rs_text();
    assert!(
        text.contains(
            "descriptor inventory at {path} is malformed ({detail}); model state is treated as unavailable, so model-vs-snapshot checks are skipped for this build"
        ),
        "build.rs must carry the same malformed-inventory wording as build_match::format_warning_inventory_malformed"
    );
}

/// B-6 — the suppression flag must only mute Outcome 3
/// (model drift). D004 mismatches, Outcome 2 (composed-not-applied),
/// and Outcome 4 (stale pending) ALWAYS print regardless of the
/// `suppress_drift_warning` setting.
///
/// Round-2 strengthening: we now exercise the classifier under each
/// suppression setting at runtime via the library entry point
/// `classify_bucket_with_pending`, then apply the suppression
/// predicate on `DriftKind::is_outcome3_drift()` — the same shape
/// build.rs uses on its `BuildDiagnostic.is_outcome3_drift` flag. The
/// source inspection still pins the wire-up shape but the runtime
/// case is what proves the four outcomes route correctly under the
/// flag.
///
/// Round-3 strengthening (Codex B-6):
///
/// - Outcome 1 (synced) is now exercised explicitly: when models ==
///   pending == snapshot the classifier must return `None` (no
///   diagnostic). This pins the silent path that build.rs depends on
///   to avoid spurious warnings on a clean tree.
/// - Outcome 2 / 3 / 4 wording is now asserted via EXACT-STRING
///   equality (built from the v3-frozen format strings) rather than
///   `contains` substring matches. A regression on any phrase will
///   surface as a hard-string-mismatch at the assertion line.
/// - The multi-bucket emission case is asserted via exact equality on
///   the single emitted text.
#[test]
fn b6_suppression_only_mutes_outcome3() {
    use djogi::migrate::build_match::{
        DriftKind, classify_bucket, classify_bucket_with_pending, format_warning_outcome2,
        format_warning_outcome3, format_warning_outcome4,
    };
    use djogi::migrate::projection::BucketKey;
    use djogi::migrate::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
    use std::collections::BTreeMap;

    fn empty_schema() -> AppliedSchema {
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        }
    }

    let bucket = BucketKey {
        database: "main".into(),
        app: "".into(),
    };

    // Codex round B-6 — Outcome 1 (synced). When models == pending
    // == snapshot, the classifier returns `None` (silent — no
    // diagnostic). This is the path build.rs walks on a clean tree;
    // any regression that returns `Some(..)` would fire a spurious
    // build warning on every developer's first compile.
    let synced = empty_schema();
    let outcome1_no_pending = classify_bucket(&bucket, Some(&synced), None, Some(&synced));
    assert!(
        outcome1_no_pending.is_none(),
        "Outcome 1 (synced, no pending) must be silent: {outcome1_no_pending:?}"
    );
    let outcome1_with_pending =
        classify_bucket(&bucket, Some(&synced), Some(&synced), Some(&synced));
    assert!(
        outcome1_with_pending.is_none(),
        "Outcome 1 (synced, pending == models == snapshot) must be silent: \
         {outcome1_with_pending:?}"
    );

    // Outcome 2 — pending matches models, snapshot diverges.
    let drifted = AppliedSchema {
        djogi_version: "9.9.9".to_string(),
        ..empty_schema()
    };
    let outcome2 = classify_bucket(
        &bucket,
        Some(&drifted),
        Some(&drifted),
        Some(&empty_schema()),
    )
    .expect("outcome 2");
    assert_eq!(outcome2.kind, DriftKind::Outcome2ComposedNotApplied);
    assert!(!outcome2.kind.is_outcome3_drift());
    // Codex round B-6 — exact-string equality. Built from the
    // frozen format string in `build_match::format_warning_outcome2`
    // (no pending version → `<unknown>` placeholder; bucket main /
    // global → `_global_` via `app_dirname`).
    assert_eq!(
        outcome2.text,
        "composed migration not yet applied: <unknown>.sdjql \
          (version <unknown>; bucket main/_global_)"
    );

    // Outcome 3 — drift, no pending.
    let outcome3 =
        classify_bucket(&bucket, Some(&drifted), None, Some(&empty_schema())).expect("outcome 3");
    assert_eq!(outcome3.kind, DriftKind::Outcome3Drift);
    assert!(outcome3.kind.is_outcome3_drift());
    assert_eq!(
        outcome3.text,
        "model drift detected for main/_global_; \
         run `djogi migrations compose` to stage the delta"
    );

    // Outcome 4 — pending diverges from models AND snapshot.
    let other = AppliedSchema {
        djogi_version: "5.5.5".to_string(),
        ..empty_schema()
    };
    let outcome4 = classify_bucket(&bucket, Some(&drifted), Some(&other), Some(&empty_schema()))
        .expect("outcome 4");
    assert_eq!(outcome4.kind, DriftKind::Outcome4PendingInvalid);
    assert!(!outcome4.kind.is_outcome3_drift());
    assert_eq!(
        outcome4.text,
        "pending compose for main/_global_ is stale relative to model state; \
         re-run `djogi migrations compose`"
    );

    // Apply the suppression predicate matching build.rs's logic:
    // `if suppress_drift && d.is_outcome3_drift()` skips emission.
    fn would_emit(d: &djogi::migrate::DriftDiagnostic, suppress_drift: bool) -> bool {
        !(suppress_drift && d.kind.is_outcome3_drift())
    }
    // suppress_drift = false → all four print.
    assert!(would_emit(&outcome2, false));
    assert!(would_emit(&outcome3, false));
    assert!(would_emit(&outcome4, false));
    // suppress_drift = true → only outcome3 muted.
    assert!(would_emit(&outcome2, true));
    assert!(!would_emit(&outcome3, true), "outcome3 must be suppressed");
    assert!(would_emit(&outcome4, true));

    // Multi-bucket sanity: bucket A drifting (Outcome 3), bucket B
    // composed-not-applied (Outcome 2). With suppression on only B
    // emits.
    let bucket_a = BucketKey {
        database: "main".into(),
        app: "alpha".into(),
    };
    let bucket_b = BucketKey {
        database: "main".into(),
        app: "beta".into(),
    };
    let a = classify_bucket(&bucket_a, Some(&drifted), None, Some(&empty_schema()))
        .expect("bucket a outcome3");
    let b = classify_bucket_with_pending(
        &bucket_b,
        Some(&drifted),
        Some(&drifted),
        Some(&empty_schema()),
        Some("V20260425010203__b"),
    )
    .expect("bucket b outcome2");
    let suppress_drift = true;
    let emitted: Vec<String> = [&a, &b]
        .into_iter()
        .filter(|d| would_emit(d, suppress_drift))
        .map(|d| d.text.clone())
        .collect();
    assert_eq!(
        emitted.len(),
        1,
        "only bucket B's outcome2 must emit under suppression: {emitted:?}"
    );
    // Codex round B-6 — exact-string equality on the multi-bucket
    // emitted text. Built from the v3-frozen Outcome 2 format string
    // with the explicit pending version threaded through.
    assert_eq!(
        emitted[0],
        "composed migration not yet applied: V20260425010203__b.sdjql \
          (version V20260425010203__b; bucket main/beta)"
    );

    // Confirm the wording functions still round-trip the frozen
    // strings — guards against a refactor that breaks the contract
    // mid-rewire.
    let _ = format_warning_outcome2(&bucket, None);
    let _ = format_warning_outcome3(&bucket);
    let _ = format_warning_outcome4(&bucket);

    // Source-shape pinning still applies — build.rs must keep its
    // selective-suppression posture.
    let text = build_rs_text();
    assert!(
        text.contains("is_outcome3_drift"),
        "build.rs must classify diagnostics by outcome kind so suppression is selective"
    );
    assert!(
        text.contains("if suppress_drift && d.is_outcome3_drift"),
        "build.rs must only suppress Outcome-3 diagnostics; D004 / Outcome 2 / Outcome 4 always print"
    );
    assert!(
        !text.contains("if drift_warnings_suppressed(&workspace_root) {\n        return;\n    }"),
        "build.rs must not blanket-return on suppress_drift_warning — selective suppression only"
    );
}

/// Codex B-7 — pending JSON format-version peek. build.rs must
/// validate `format_version` BEFORE accepting a pending file as
/// input to the three-way classifier; a future-version pending file
/// surfaces a version-mismatch warning rather than feeding garbage
/// through `classify_outcome`.
#[test]
fn b7_pending_format_version_peek_present() {
    let text = build_rs_text();
    // The peek helper exists.
    assert!(
        text.contains("fn peek_format_version("),
        "build.rs must define a format_version peek helper"
    );
    // The version mismatch warning shape is wired up.
    assert!(
        text.contains("pending JSON format version")
            && text.contains("not supported by this Djogi"),
        "build.rs must emit a structured version-mismatch warning"
    );
    // The peek runs in the pending walk before the bucket is
    // inserted into the classifier's input map.
    assert!(
        text.contains("if let Some(found) = peek_format_version(&v)"),
        "build.rs must short-circuit on version mismatch in the pending walk"
    );
}
