//! Spatial compile-pass trybuild fixtures.
//!
//! trybuild copies this crate's features into a generated fixture crate, but
//! strips dependency-feature forwards like `djogi/spatial` from the generated
//! feature definitions. Djogi's spatial feature has no extra dependencies
//! today, so this binary enables the equivalent rustc cfg for the generated
//! crate and its child dependency build.

use trybuild::TestCases;

fn enable_spatial_cfg_for_child_cargo() {
    const SEP: char = '\x1f';
    const FEATURE: &str = "feature=\"spatial\"";

    let existing = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    if existing.split(SEP).any(|arg| arg == FEATURE) {
        return;
    }

    let addition = format!("--cfg{SEP}{FEATURE}");
    let encoded = if existing.is_empty() {
        addition
    } else {
        format!("{existing}{SEP}{addition}")
    };

    // This test binary has a single test, so no sibling test thread observes
    // a partially-mutated environment. The value only affects child cargo
    // processes spawned by trybuild from this process.
    unsafe {
        std::env::set_var("CARGO_ENCODED_RUSTFLAGS", encoded);
    }
}

#[test]
fn compile_pass_spatial() {
    enable_spatial_cfg_for_child_cargo();

    let t = TestCases::new();
    t.pass("tests/compile_pass/phase6_spatial_field.rs");
    t.pass("tests/compile_pass/phase6_spatial_query.rs");
    t.pass("tests/compile_pass/phase6_5_spatial_models.rs");
    t.pass("tests/compile_pass/phase7_zero2_option_geopoint_spatial.rs");
}
