//! Library-level entrypoint unit tests for T-PARSE (issue #370 Task 10).
//!
//! These tests exercise `djogi_cli::run_with_args` and
//! `djogi_cli::run_with_provider` WITHOUT spawning a subprocess or
//! connecting to a database. The observable stub provider counts
//! `models()` calls via `AtomicUsize`, making the "schema with empty
//! provider" test non-vacuous: it proves the dispatch path actually
//! consults the threaded provider.

use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use djogi::apps::{AppDescriptor, AppRegistry};
use djogi::descriptor::{DeferrabilitySpec, EnumDescriptor, ModelDescriptor};
use djogi::migrate::DescriptorProvider;

/// Stub [`DescriptorProvider`] that counts how many times `models()`
/// is invoked. Returns empty vectors for all four descriptor streams
/// so descriptor-dependent commands hit the zero-model diagnostic path.
///
/// The atomic counter is the key innovation: without it, the "schema
/// with empty provider" test could only observe the exit code and
/// would be vacuous (any code path returning a non-success exit would
/// satisfy it). By asserting `counter > 0`, we prove the dispatch
/// actually walked through our provider.
struct ObservableProvider {
    models_called: AtomicUsize,
}

impl ObservableProvider {
    fn new() -> Self {
        Self {
            models_called: AtomicUsize::new(0),
        }
    }

    /// Return the number of times `models()` was called since construction.
    fn models_call_count(&self) -> usize {
        self.models_called.load(Ordering::SeqCst)
    }
}

impl DescriptorProvider for ObservableProvider {
    fn models(&self) -> Vec<&'static ModelDescriptor> {
        self.models_called.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }

    fn enums(&self) -> Vec<&'static EnumDescriptor> {
        Vec::new()
    }

    fn apps(&self) -> &'static [AppDescriptor] {
        AppRegistry::all()
    }

    fn deferrability_specs(&self) -> Vec<&'static DeferrabilitySpec> {
        Vec::new()
    }
}

/// Unknown subcommands produce a non-success exit via clap error handling.
///
/// This verifies that the parsing layer returns a failure exit code for
/// unrecognized commands, before any descriptor provider is consulted.
#[test]
fn unknown_subcommand_returns_failure() {
    let result = djogi_cli::run_with_args(["djogi", "definitely_not_a_real_subcommand"]);
    assert_eq!(
        result,
        ExitCode::from(2),
        "clap parse error must map to exit 2 (refusal), not exit 1 (runtime error)"
    );
}

/// The `--help` flag produces a success exit and does not consult the
/// provider.
///
/// This verifies that clap handles help before any descriptor provider
/// is threaded through, confirming the help path is purely structural.
#[test]
fn help_flag_returns_success() {
    let result = djogi_cli::run_with_args(["djogi", "--help"]);
    assert_eq!(result, ExitCode::SUCCESS, "--help should succeed");
}

/// The `schema` command consults the threaded [`DescriptorProvider`]
/// and returns a non-success exit when no models are registered.
///
/// The observable stub proves the dispatch path calls `provider.models()`:
/// the counter must be greater than zero. Combined with a non-success exit,
/// this confirms the schema path hits the zero-model diagnostic rather than
/// silently succeeding or failing for another reason.
#[test]
fn schema_with_empty_provider_returns_failure_and_consults_provider() {
    let provider = ObservableProvider::new();
    let result = djogi_cli::run_with_provider(["djogi", "schema", "--format", "json"], &provider);

    // Non-success signals the zero-model diagnostic path.
    assert_eq!(
        result,
        ExitCode::from(2),
        "empty provider must trigger zero-descriptor refusal (exit 2), not runtime error (exit 1)"
    );

    // Provider.models() must have been called at least once. Without this
    // assertion, the test is vacuous — any code path returning non-success
    // would pass, including one that never consulted the provider.
    assert!(
        provider.models_call_count() > 0,
        "provider.models() should be called during schema dispatch; \
         got {} calls",
        provider.models_call_count()
    );
}

/// Create a fresh, empty temp directory (no `migrations/` tree) and return
/// its path. Used to exercise the `verify` zero+zero refusal in-process:
/// the gate runs before any config load or DB connection, so an empty
/// workspace with an empty provider is enough to drive the refusal without
/// Postgres.
fn fresh_empty_workspace(stub: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_canonical = std::env::temp_dir().canonicalize().expect("canonicalize temp_dir");
    let dir = temp_canonical.join(format!("djogi-370-{stub}-{stamp}"));
    assert!(
        dir.starts_with(&temp_canonical),
        "refusing to create dir outside temp: {}",
        dir.display()
    );
    std::fs::create_dir_all(&dir).expect("create temp workspace");
    dir
}

/// `migrations verify` refuses with exit 2 when there are NEITHER
/// descriptors NOR on-disk snapshots (§5.6 / REQ-370-8).
///
/// This is the dual-cause refusal that mirrors the compose/schema/docs
/// gates: a standalone binary with an empty inventory and no snapshot tree
/// has nothing to verify against, so it refuses rather than reporting
/// vacuous success. The gate runs before config load, so an empty provider
/// against an empty workspace drives it without a database. The observable
/// counter proves the dispatch consulted the provider.
#[test]
fn verify_with_empty_provider_and_no_snapshots_refuses_exit_2() {
    let provider = ObservableProvider::new();
    let workspace = fresh_empty_workspace("verify-zero-zero");
    let result = djogi_cli::run_with_provider(
        [
            "djogi",
            "migrations",
            "verify",
            "--workspace",
            workspace.to_str().expect("utf8 temp path"),
        ],
        &provider,
    );

    assert_eq!(
        result,
        ExitCode::from(2),
        "verify with no descriptors AND no snapshots must refuse (exit 2)"
    );
    assert!(
        provider.models_call_count() > 0,
        "verify dispatch must consult provider.models(); got {} calls",
        provider.models_call_count()
    );

    let temp_canonical = std::env::temp_dir().canonicalize().expect("canonicalize temp_dir");
    assert!(
        workspace.starts_with(&temp_canonical),
        "refusing to remove dir outside temp: {}",
        workspace.display()
    );
    let _ = std::fs::remove_dir_all(&workspace);
}
