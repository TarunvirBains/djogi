//! Adopter-crate isolation guard for Cluster 8δ T7.4 macro path-routing.
//!
//! # Why this test exists
//!
//! `djogi-macros/tests/compile_pass/phase8_t7_4_punnu_boot_hook_emitted.rs`
//! and friends run inside trybuild. Trybuild compiles each fixture as a
//! standalone crate but with the same dependency graph as the test
//! crate that drives trybuild — `djogi-macros`. `djogi-macros/Cargo.toml`
//! lists `sassi`, `serde`, and `serde_json` as `[dev-dependencies]`
//! for unrelated fixtures (8γ T6.10's lookup-op no-regex lock, the
//! JsonbSchema fixtures' `serde::Serialize` derives), so the
//! compile_pass bucket has those crates reachable via direct
//! `extern crate` resolution. A future macro-emission regression that
//! spelled `::sassi::*` / `::heeranjid::*` / `::time::*` etc. directly
//! instead of routing through `::djogi::*` per
//! `feedback_macro_path_routing.md` would compile inside that bucket
//! — the bucket can't catch the bug.
//!
//! # What this test does
//!
//! Shells out `cargo check --all-targets --manifest-path` against
//! `tests/adopter_crate_isolation/Cargo.toml`, a one-binary fixture
//! crate whose `[dependencies]` table contains exactly one entry —
//! `djogi`. The fixture has its own `[workspace]` so cargo's path-dep
//! resolution does NOT walk back up to djogi's outer workspace and
//! absorb the fixture as a member; the dep graph really does carry
//! only `djogi` and its transitive tree.
//!
//! A macro-emission regression that introduces a `::sassi::*` /
//! `::heeranjid::*` / `::time::*` / `::uuid::*` / `::inventory::*` /
//! `::serde::*` / `::tokio::*` / `::tokio_postgres::*` direct path fails the
//! `cargo check` invocation with `error[E0433]: failed to resolve: use
//! of undeclared crate or module \`<crate>\``. We capture stdout +
//! stderr and panic with the full output so the regression is
//! diagnosable from the test harness alone — no need to re-run the
//! command by hand.
//!
//! # Build artifact location
//!
//! `--target-dir` points at `<workspace-root>/target/adopter_crate_isolation`
//! so build output lands inside the gitignored `target/` tree, never in
//! the fixture source dir. The fixture crate carries its own
//! committed `Cargo.lock` so the isolated dep graph is reproducible.
//! The fixture-local `.gitignore` covers `target/` as a
//! belt-and-suspenders guard for direct `cargo check` invocations.
//!
//! # Concurrency
//!
//! The first invocation compiles `djogi` from scratch into the fixture
//! target dir; subsequent runs hit cargo's incremental cache. Cargo
//! handles its own internal locking on the target dir, so running this
//! test concurrently with other tests that share `<workspace-root>/target`
//! is safe — the fixture's target subdir is isolated.
//!
//! Spec anchor:
//!   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//!   §3 commit T7.4 — "Trybuild fixture" bullet (this driver invokes the
//!   stronger sibling complementing the same-named compile_pass fixture).
//!
//! GitHub: djogi#124.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn fixture_compiles_with_only_djogi_as_direct_dep() {
    // `CARGO_MANIFEST_DIR` resolves to the `djogi-macros/` crate root
    // when this integration test runs (cargo's standard convention).
    // The workspace root is its parent — djogi-macros is a top-level
    // member of djogi's workspace.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap_or_else(|| {
        panic!(
            "CARGO_MANIFEST_DIR ({}) has no parent — expected djogi-macros to live \
             directly under djogi's workspace root",
            manifest_dir.display(),
        )
    });

    let fixture_dir = manifest_dir.join("tests").join("adopter_crate_isolation");
    let fixture_manifest = fixture_dir.join("Cargo.toml");

    assert!(
        fixture_manifest.is_file(),
        "fixture Cargo.toml not found at {} — the adopter-isolation fixture is missing \
         from the worktree",
        fixture_manifest.display(),
    );

    // Park build artifacts under the workspace `target/` tree so they
    // are gitignored. A dedicated subdirectory keeps the fixture's
    // incremental cache distinct from the outer workspace's build
    // output (the fixture's dep graph is intentionally narrower; mixing
    // caches would not produce correctness bugs but could cause
    // confusing rebuilds).
    let target_dir = workspace_root
        .join("target")
        .join("adopter_crate_isolation");

    // Use the cargo binary that drove this test invocation — cargo
    // sets `CARGO` for spawned processes so toolchain selection
    // (rustup overrides, +nightly, etc.) flows through to the child.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let output = Command::new(&cargo)
        .arg("check")
        .arg("--all-targets")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(&fixture_manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        // `CARGO_TARGET_DIR` from the parent `cargo test` invocation
        // would otherwise leak into the child cargo process. `--target-dir`
        // takes precedence per cargo's CLI > env > config rule, but
        // explicitly clearing the env var keeps the child's behaviour
        // independent of how the parent was launched.
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "failed to spawn `{} check --all-targets --locked --manifest-path {} --target-dir {}`: {}",
                PathBuf::from(&cargo).display(),
                fixture_manifest.display(),
                target_dir.display(),
                err,
            )
        });

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "adopter_crate_isolation fixture failed to compile.\n\
             \n\
             A locked `cargo check --all-targets` against a fixture crate that depends on `djogi` \
             alone could not resolve a name emitted by a macro expansion. \
             The likely cause is a macro-emitted path that spells `::sassi::*` / \
             `::heeranjid::*` / `::time::*` / `::uuid::*` / `::inventory::*` / \
             `::serde::*` / `::tokio::*` / `::tokio_postgres::*` / \
             `::postgres_types::*` etc. \
             directly instead of routing through `::djogi::*` per \
             `feedback_macro_path_routing.md`. Inspect the stderr below for the \
             offending E0433 — the path appears in a macro expansion, not in \
             handwritten fixture code.\n\
             \n\
             cargo:         {}\n\
             manifest:      {}\n\
             target-dir:    {}\n\
             exit status:   {}\n\
             \n\
             ────── stdout ──────\n{}\n\
             ────── stderr ──────\n{}\n",
            PathBuf::from(&cargo).display(),
            fixture_manifest.display(),
            target_dir.display(),
            output.status,
            stdout,
            stderr,
        );
    }
}
