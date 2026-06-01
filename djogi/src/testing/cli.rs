//! Shared helpers for CLI integration tests under
//! `djogi-cli/tests/internal/`.
//! These functions back the integration tests that spawn the compiled
//! `djogi` binary as a subprocess (`djogi verify`, `djogi analyze`, …)
//! and seed a per-test database via `#[djogi_test]`. Before this module
//! existed, every CLI integration test re-declared its own copy of the
//! same four helpers; centralising them here lets each test file focus
//! on the scenario under test and keeps the binary-locating, workspace
//! provisioning, and `Djogi.toml`-writing logic in one auditable place.
//! # Surface visibility
//! Gated behind `cfg(any(test, feature = "testing"))` (matching the
//! existing convention used by other `testing`-feature surfaces in this
//! crate). Adopter integration test crates that consume these helpers
//! must enable djogi's `testing` feature on their dev dependency on
//! `djogi`.
//! # Raw access
//! [`current_database`] uses [`crate::__bypass::RawAccessExt::raw_scalar`]
//! to issue `SELECT current_database` against the per-test
//! [`crate::DjogiContext`]. The use is internal to djogi (no `raw_*`
//! method appears at the call site outside this crate), so adopter test
//! files do not need a `#[deliberately_bypass_convention_with_raw_sql]`
//! attribute solely on account of these helpers.

#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::{Path, PathBuf};

use crate::__bypass::RawAccessExt as _;
use crate::DjogiContext;

/// Walk from the running test executable to the workspace's compiled
/// `djogi` binary.
/// Test binaries live at `target/<profile>/deps/<test_name>-<hash>`; the
/// CLI binary lives at `target/<profile>/djogi`. We walk up one level
/// (drop the test-binary file) to reach `deps/`, then up another level to
/// reach `<profile>/`, then join `djogi`.
/// # Why the `option_env!` branch is structurally retained
/// Cargo only sets `CARGO_BIN_EXE_<name>` for integration tests in the
/// same package as the matching `[[bin]]`. In today's `djogi-cli`
/// tests this helper is compiled by the `djogi` crate, so the env-var
/// branch is expected to be inactive and the walk fallback is
/// load-bearing. The branch is preserved verbatim so the helper's
/// surface stays identical to the per-file copies it replaces, and so
/// a future relocation that compiles this code in the binary package
/// can use Cargo's direct path without touching call sites. The walk
/// reliably finds the binary because Cargo builds the package's
/// `[[bin]]` targets before its `[[test]]` targets when running
/// `cargo test -p djogi-cli`.
pub fn djogi_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_djogi") {
        return PathBuf::from(path);
    }

    let exe = std::env::current_exe().expect("current_exe");
    // exe = target/<profile>/deps/<test>-<hash>
    let deps = exe.parent().expect("current_exe has parent (deps/)");
    let profile_dir = deps.parent().expect("deps has parent (profile/)");
    profile_dir.join("djogi")
}

/// Resolve the connected test context's `current_database` so callers
/// can splice the per-test database name into a fixture `Djogi.toml`
/// URL.
/// Issues a single `SELECT current_database::text` against `ctx` via
/// the in-crate raw escape hatch. The cast to `text` is deliberate
/// `current_database` returns a `name`, and decoding `name` directly
/// into Rust's `String` is fragile across `tokio_postgres` versions.
pub async fn current_database(ctx: &mut DjogiContext) -> String {
    ctx.raw_scalar::<String>("SELECT current_database()::text", &[])
        .await
        .expect("current_database")
}

/// Build a unique temporary workspace directory and return its path.
/// The directory is named `djogi-{tag}-{nanos-since-epoch}-{counter}`
/// under the platform's temp dir, where `counter` is a process-local
/// monotonic value. The combination of nanos + counter keeps every
/// invocation distinct even under heavy parallel test scheduling.
/// Callers should pass a `tag` that incorporates both their test-suite
/// identity (e.g. `t9-7-verify`) and the per-case discriminator
/// (e.g. `clean`, `mismatch`). The full string lands verbatim in the
/// directory name and is the easiest way to attribute a stale temp dir
/// back to the test that left it on disk.
pub fn temp_workspace(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("djogi-{tag}-{stamp}-{n}"));
    fs::create_dir_all(&p).expect("create_dir_all temp workspace");
    p
}

/// Write a minimal `Djogi.toml` in `workspace` whose `database.url`
/// points at the supplied per-test URL.
/// The CLI subcommands these tests drive (`djogi verify`,
/// `djogi analyze`, …) read this file via
/// `DjogiConfig::load_from_workspace`. The `[server]` block is required
/// by the loader; `port = 0` lets the OS pick an ephemeral port if the
/// loaded config later spawns a server.
pub fn write_minimal_djogi_toml(workspace: &Path, db_url: &str) {
    let toml = format!(
        r#"profile = "development"

[database]
url = "{db_url}"

[server]
host = "127.0.0.1"
port = 0
"#,
    );
    fs::write(workspace.join("Djogi.toml"), toml).expect("write Djogi.toml");
}
