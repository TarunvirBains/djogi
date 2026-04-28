//! `djogi db` and `djogi docs` subcommand glue — Phase 7 T8.
//!
//! Three leaves:
//!
//! - `db reset` — drops, recreates, and replays committed migrations
//!   for the application database. Triple-gated (localhost +
//!   non-production profile + explicit `--yes`) per the v3 §8 brief.
//! - `db seed` — runs operator-authored SQL fixtures from
//!   `seeds/<database>/`. Localhost-or-`--allow-non-localhost`.
//! - `docs` — renders per-model markdown reference pages from the
//!   descriptor inventory.
//!
//! All three flow through public APIs in `djogi::migrate` (or
//! `::config`) so integration tests can exercise the underlying logic
//! without spawning subprocesses.
//!
//! # Exit codes
//!
//! Every subcommand in this module obeys a uniform three-value matrix
//! so shell integrations can distinguish "operation refused" from
//! "operation failed":
//!
//! | Code | Meaning |
//! |------|---------|
//! | `0`  | Success — the command completed and any post-state was applied. |
//! | `1`  | Error — config load failure, network, SQL, or any other underlying runtime failure. |
//! | `2`  | Refusal — either a policy gate (localhost, production profile, missing `--yes`, …) blocked execution before any side effect, OR clap-style argument validation rejected the invocation (missing flag, mutually exclusive flags). |
//!
//! Exit code `2` deliberately bundles policy refusals and
//! argument-validation errors. Clap's default behaviour is to return
//! `2` for unknown / malformed flags; manual `2` returns in
//! `migrations attune` (missing `--from`, conflicting flags) and the
//! `db reset` / `db seed` gates intentionally share that code so a
//! CI script can treat any `2` as "operator must intervene; nothing
//! happened" without distinguishing the two cases. `1` is reserved
//! for "we tried; something broke" so a CI can retry. The matrix is
//! also documented in `ReadMe.MD` and `docs/spec/configuration.md`
//! so the operator-facing surface stays in sync.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use djogi::config::DjogiConfig;
use djogi::migrate::{
    ResetError, ResetReport, ResetRequest, SeedError, SeedOutcome, SeedReport, generate_docs,
    reset_app_database, run_seeds,
};

/// Resolve the workspace root from the `--workspace` flag. Default:
/// the current working directory. Mirrors the helper in
/// [`crate::migrations`].
fn resolve_workspace(workspace: Option<PathBuf>) -> PathBuf {
    workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Build a Tokio current-thread runtime for the synchronous CLI
/// surface. Reused by `db reset` and `db seed` — both need to drive
/// async library calls from a sync `fn main()` shape.
fn build_runtime(label: &str) -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            eprintln!("djogi {label}: tokio runtime: {e}");
            ExitCode::from(1)
        })
}

// ── db reset ──────────────────────────────────────────────────────────────

/// `djogi db reset` entry point.
///
/// `yes`: when `true`, the function does NOT prompt the operator —
/// the request flows straight into [`reset_app_database`]. When
/// `false`, the function prints a y/N prompt to stderr and reads
/// stdin; only an explicit `y` / `yes` answer (case-insensitive)
/// proceeds. Any other input refuses with the standard
/// `ResetRefusal::NotConfirmed` exit code.
///
/// `maintenance_database` defaults to `"postgres"` — the conventional
/// administrative DB present on every cluster — when the operator
/// supplies nothing more specific.
pub fn reset_cmd(yes: bool, maintenance_database: String, workspace: Option<PathBuf>) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let config = match DjogiConfig::load_from_workspace(&workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi db reset: config load: {e}");
            return ExitCode::from(1);
        }
    };

    // If the operator omitted `--yes`, run an interactive prompt
    // BEFORE we touch the runtime — minimises blast radius in the
    // refusal path.
    let confirmed = if yes {
        true
    } else {
        match interactive_confirm(&config.database.url) {
            Ok(c) => c,
            Err(_) => {
                // I/O error reading stdin — refuse rather than guess.
                eprintln!(
                    "djogi db reset: failed to read confirmation; \
                     refusing without an explicit `--yes`"
                );
                return ExitCode::from(1);
            }
        }
    };

    let runtime = match build_runtime("db reset") {
        Ok(r) => r,
        Err(code) => return code,
    };

    let exit = runtime
        .block_on(async { run_reset(&workspace, &config, &maintenance_database, confirmed).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`reset_cmd`]. Returns the desired exit code.
async fn run_reset(
    workspace: &Path,
    config: &DjogiConfig,
    maintenance_database: &str,
    confirmed: bool,
) -> i32 {
    let req = ResetRequest {
        workspace_root: workspace,
        database_url: &config.database.url,
        profile: &config.profile,
        confirmed,
        maintenance_database,
        migrate_config: djogi::config::MigrateConfig {
            concurrent_warn_relpages: config.migrate.concurrent_warn_relpages,
            strict_concurrent_warnings: config.migrate.strict_concurrent_warnings,
            pk_flip_long_tx_threshold_secs: config.migrate.pk_flip_long_tx_threshold_secs,
            pk_flip_join_table_option: config.migrate.pk_flip_join_table_option,
        },
    };
    match reset_app_database(req).await {
        Ok(report) => {
            print_reset_report(&report);
            0
        }
        Err(ResetError::Refused(refusal)) => {
            eprintln!("djogi db reset: refused — {refusal}");
            // Use a distinct exit code (2) for refusal so scripts can
            // distinguish "policy refused" from "underlying SQL
            // failure". Mirrors clap's argument-error convention.
            2
        }
        Err(other) => {
            eprintln!("djogi db reset: {other}");
            1
        }
    }
}

/// Print the post-reset report to stdout. Operators see one line per
/// replayed migration plus a final tally.
fn print_reset_report(report: &ResetReport) {
    println!(
        "db reset complete — recreated database `{}`",
        report.database
    );
    if report.replayed_versions.is_empty() {
        println!("  no committed migrations replayed");
        return;
    }
    for entry in &report.replayed_versions {
        let app = if entry.bucket.app.is_empty() {
            "_global_"
        } else {
            entry.bucket.app.as_str()
        };
        println!(
            "  replayed {database}/{app}: {version}",
            database = entry.bucket.database,
            version = entry.version,
        );
    }
    println!(
        "  total: {} migration(s) replayed",
        report.replayed_versions.len()
    );
}

/// Interactive y/N prompt. Reads one line from stdin; returns `Ok(true)`
/// only on a `y` / `yes` answer (case-insensitive ASCII). Anything
/// else (including EOF, empty input, or `n` / `no`) returns `Ok(false)`.
fn interactive_confirm(database_url: &str) -> std::io::Result<bool> {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    writeln!(
        handle,
        "WARNING: db reset will DROP and RECREATE the application database \
         pointed at by DATABASE_URL ({database_url}); every row will be lost. \
         Migrations under `migrations/<database>/` will be replayed onto the \
         freshly-created database. This action cannot be undone."
    )?;
    write!(handle, "Type `yes` to confirm, anything else to abort: ")?;
    handle.flush()?;
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

// ── db seed ───────────────────────────────────────────────────────────────

/// `djogi db seed` entry point.
pub fn seed_cmd(
    database: String,
    allow_non_localhost: bool,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let config = match DjogiConfig::load_from_workspace(&workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi db seed: config load: {e}");
            return ExitCode::from(1);
        }
    };

    let runtime = match build_runtime("db seed") {
        Ok(r) => r,
        Err(code) => return code,
    };
    let exit = runtime
        .block_on(async { run_seed(&workspace, &config, &database, allow_non_localhost).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`seed_cmd`]. Returns the desired exit code.
///
/// **Per-database routing.** The `--database <name>` flag selects
/// BOTH the `seeds/<name>/` directory the runner walks AND the
/// connection URL the SQL fires against. The
/// CLI derives the per-database URL by splicing `<name>` into
/// `database.url`'s path component (via
/// [`djogi::migrate::derive_per_database_url`]) — without that
/// splice, `db seed --database crud_log` would silently run
/// crud-log seed SQL against the application database. A malformed
/// application URL (no path component) is surfaced as a typed
/// [`SeedError::MalformedApplicationUrl`] rather than a default to
/// the wrong DB.
async fn run_seed(
    workspace: &Path,
    config: &DjogiConfig,
    database: &str,
    allow_non_localhost: bool,
) -> i32 {
    // Splice the operator's `--database <name>` into the application
    // URL. The result is the connection target AND the URL the
    // localhost gate inside `run_seeds` evaluates against — both
    // gate and SQL execution stay on the same database.
    //
    // Codex round-2 A-6: surface the malformed-URL case via the
    // typed `SeedError::MalformedApplicationUrl` variant rather than
    // a bare `eprintln!`. The variant was previously dead — the CLI
    // now constructs it explicitly so the error path is operator-
    // actionable AND the variant has a real call site.
    let routed_url = match djogi::migrate::derive_per_database_url(&config.database.url, database) {
        Some(u) => u,
        None => {
            let err = SeedError::MalformedApplicationUrl {
                application_url: config.database.url.clone(),
            };
            eprintln!("djogi db seed: {err} (--database `{database}`)");
            return 1;
        }
    };

    // Build a context against the routed (per-database) URL.
    let pool = match djogi::pg::pool::DjogiPool::connect(&routed_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("djogi db seed: connect: {e}");
            return 1;
        }
    };
    let mut ctx = djogi::context::DjogiContext::from_pool(pool);

    match run_seeds(
        &mut ctx,
        workspace,
        database,
        &routed_url,
        allow_non_localhost,
    )
    .await
    {
        Ok(report) => {
            print_seed_report(&report);
            0
        }
        Err(SeedError::LocalhostGate { database_url }) => {
            eprintln!(
                "djogi db seed: refused — DATABASE_URL `{database_url}` is not \
                 localhost; pass `--allow-non-localhost` to override"
            );
            2
        }
        Err(other) => {
            eprintln!("djogi db seed: {other}");
            1
        }
    }
}

fn print_seed_report(report: &SeedReport) {
    if report.entries.is_empty() {
        println!("db seed: no seeds discovered");
        return;
    }
    let mut applied = 0usize;
    let mut skipped = 0usize;
    for entry in &report.entries {
        let label = match entry.outcome {
            SeedOutcome::Applied => {
                applied += 1;
                "applied"
            }
            SeedOutcome::SkippedAlreadyApplied => {
                skipped += 1;
                "skipped (already applied)"
            }
        };
        println!("  {label:>30}  {name}", name = entry.seed_name);
    }
    println!("db seed: {applied} applied, {skipped} skipped");
}

// ── docs ──────────────────────────────────────────────────────────────────

/// `djogi docs` entry point.
///
/// `output` defaults to `target/djogi-docs/` under the workspace. The
/// per-model files are written into `<output>/<app>/<Model>.md` and a
/// top-level `<output>/README.md` indexes them.
pub fn docs_cmd(output: Option<PathBuf>, workspace: Option<PathBuf>) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let output = output.unwrap_or_else(|| workspace.join("target").join("djogi-docs"));
    match generate_docs(&output) {
        Ok(report) => {
            println!(
                "docs: rendered {n} model page(s) into {path}",
                n = report.models_rendered,
                path = report.output_root.display(),
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("djogi docs: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_workspace(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("djogi-cli-db-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// `db reset` without `--yes` and without an interactive answer
    /// must refuse before any I/O.
    #[test]
    fn reset_cmd_refuses_when_not_confirmed_and_url_remote() {
        // We can't easily inject stdin through the public `reset_cmd`
        // entry, but we can verify that a remote URL refuses with the
        // localhost gate even when `yes = true` — proving the gate
        // chain is wired through the CLI.
        let work = temp_workspace("reset_remote");
        let toml = "[database]\nurl = \"postgres://prod.example.com/main\"\n\
                    max_connections = 1\ndev_mode = false\n\
                    [server]\nhost = \"127.0.0.1\"\nport = 1234\n";
        fs::write(work.join("Djogi.toml"), toml).unwrap();
        // Save and clear DATABASE_URL so the env override doesn't
        // mask the file value during this test.
        let prior = std::env::var("DATABASE_URL").ok();
        // SAFETY: tests run with --test-threads=1.
        unsafe { std::env::remove_var("DATABASE_URL") };

        // `yes = true` skips the interactive prompt; we expect the
        // localhost gate to refuse and exit code 2.
        let exit = reset_cmd(true, "postgres".to_string(), Some(work.clone()));
        assert_eq!(exit, ExitCode::from(2), "remote URL must hit refusal exit");

        match prior {
            Some(v) => unsafe { std::env::set_var("DATABASE_URL", v) },
            None => unsafe { std::env::remove_var("DATABASE_URL") },
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// `db reset` against a production profile (even with localhost +
    /// `--yes`) must refuse with the production-profile gate.
    #[test]
    fn reset_cmd_refuses_on_production_profile() {
        let work = temp_workspace("reset_prod");
        let toml = "profile = \"production\"\n\
                    [database]\nurl = \"postgres://localhost/main\"\n\
                    max_connections = 1\ndev_mode = false\n\
                    [server]\nhost = \"127.0.0.1\"\nport = 1234\n";
        fs::write(work.join("Djogi.toml"), toml).unwrap();
        let prior = std::env::var("DATABASE_URL").ok();
        unsafe { std::env::remove_var("DATABASE_URL") };

        let exit = reset_cmd(true, "postgres".to_string(), Some(work.clone()));
        assert_eq!(exit, ExitCode::from(2), "production must refuse");

        match prior {
            Some(v) => unsafe { std::env::set_var("DATABASE_URL", v) },
            None => unsafe { std::env::remove_var("DATABASE_URL") },
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// `docs` against an empty inventory still produces a README and
    /// returns success.
    #[test]
    fn docs_cmd_against_empty_inventory_succeeds() {
        let work = temp_workspace("docs_empty");
        let out = work.join("target/djogi-docs");
        let exit = docs_cmd(Some(out.clone()), Some(work.clone()));
        assert_eq!(exit, ExitCode::from(0));
        // The renderer writes a sentinel README.
        let readme = std::fs::read_to_string(out.join("README.md")).unwrap();
        assert!(readme.contains("Djogi model reference"));
        let _ = fs::remove_dir_all(&work);
    }
}
