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
    let result = djogi_cli::run_with_args(&[
        String::from("djogi"),
        String::from("definitely_not_a_real_subcommand"),
    ]);
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
    let result = djogi_cli::run_with_args(&[String::from("djogi"), String::from("--help")]);
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
    let result = djogi_cli::run_with_provider(
        &[
            String::from("djogi"),
            String::from("schema"),
            String::from("--format"),
            String::from("json"),
        ],
        &provider,
    );

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
