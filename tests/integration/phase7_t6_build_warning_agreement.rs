//! Phase 7 T6 — build.rs warning text agreement test.
//!
//! Two independent code paths emit the exact same warning text:
//!
//! 1. [`djogi::migrate::build_match`] — production code, used by the
//!    `migrations status` and library callers.
//! 2. `djogi/build.rs` — the compile-time build script that surfaces
//!    drift via `cargo:warning=`. Build scripts cannot import the
//!    crate they're building, so the wording is duplicated.
//!
//! This test is the "trybuild-style expectation" called out in the
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
    let lib = djogi::migrate::build_match::format_warning_outcome2(&bucket);
    assert_eq!(lib, "composed migration not yet applied: main/billing");
    let text = build_rs_text();
    // The build.rs's frozen-string format helper must contain the
    // same template literal modulo placeholders.
    assert!(
        text.contains("composed migration not yet applied: {database}/{app}"),
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
fn d004_unregistered_wording_matches_build_rs() {
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
fn d004_missing_wording_matches_build_rs() {
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
