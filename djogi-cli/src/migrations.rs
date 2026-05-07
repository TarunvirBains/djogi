//! `djogi migrations` subcommand glue — Phase 7 T6.
//!
//! Two leaves: `compose` and `status`. Both flow through the public
//! `djogi::migrate` API. Compose acquires the workspace file lock for
//! the duration of the call; status is read-only and does not.
//!
//! The CLI surface here is intentionally thin — all the real logic
//! lives in the library so integration tests can exercise it without
//! spawning subprocesses.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use djogi::apps::AppRegistry;
use djogi::migrate::{
    AppLifecycle, AttuneError, AttuneMode, AttuneRequest, ComposeError, ComposeRequest,
    GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME, acquire_workspace_lock, attune, compose,
    project_from_inventory,
};

/// Resolve the workspace root from the `--workspace` flag. When the
/// flag is absent we use the current working directory — the typical
/// invocation pattern is `cd <project>` then `djogi migrations …`.
fn resolve_workspace(workspace: Option<PathBuf>) -> PathBuf {
    workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Walk the on-disk `migrations/<database>/<app>/` tree and return the
/// set of buckets that already have a `schema_snapshot.json` file.
///
/// Compose's `snapshots` map must include the OLD bucket of any
/// renamed app — and that bucket is guaranteed to be absent from the
/// current `models` inventory because the `#[app(renamed_from =
/// "old")]` annotation lives on the NEW app. Walking disk directly
/// recovers those orphaned snapshots so the differ sees both sides of
/// a rename.
///
/// Each entry maps to a [`djogi::migrate::projection::BucketKey`]
/// using the inverse of [`djogi::migrate::app_dirname`] (synthetic
/// `_global_` directory → empty-string label).
fn discover_snapshot_buckets_on_disk(
    workspace: &Path,
) -> Vec<djogi::migrate::projection::BucketKey> {
    let mut out = Vec::new();
    let migrations_root = djogi::migrate::migrations_root(workspace);
    let Ok(db_entries) = std::fs::read_dir(&migrations_root) else {
        return out;
    };
    for db_entry in db_entries.flatten() {
        let Ok(ft) = db_entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let Some(database) = db_entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(app_entries) = std::fs::read_dir(db_entry.path()) else {
            continue;
        };
        for app_entry in app_entries.flatten() {
            let Ok(ft) = app_entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let Some(dirname) = app_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let snap_path = app_entry.path().join("schema_snapshot.json");
            if !snap_path.exists() {
                continue;
            }
            let label = djogi::migrate::app_label_from_dirname(&dirname).to_string();
            out.push(djogi::migrate::projection::BucketKey {
                database: database.clone(),
                app: label,
            });
        }
    }
    out
}

/// `djogi migrations compose` entry point.
pub fn compose_cmd(
    name: &str,
    allow_destructive: bool,
    force_overwrite: bool,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let models = match project_from_inventory() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("djogi migrations compose: projection error: {e}");
            return ExitCode::from(1);
        }
    };
    let apps: Vec<AppLifecycle> = AppRegistry::all()
        .iter()
        .map(|d| AppLifecycle {
            label: d.label.to_string(),
            database: d.database.to_string(),
            renamed_from: d.renamed_from.map(str::to_string),
            tombstone: d.tombstone,
        })
        .collect();
    // Codex round-2 A-1: the resolved workspace flows into config
    // loading too. Round-2 / B-12 update: compose now consumes the
    // [`MigrateConfig::pk_flip_join_table_option`] knob so we no
    // longer drop the parsed config — the join-table layout
    // selected in `Djogi.toml` reaches the differ via this path.
    let djogi_config = match djogi::config::DjogiConfig::load_from_workspace(&workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations compose: config load: {e}");
            return ExitCode::from(1);
        }
    };
    let pk_flip_option = djogi::migrate::PkFlipJoinTableOption::from_config_char(
        djogi_config.migrate.pk_flip_join_table_option,
    );
    compose_with_inputs(
        &workspace,
        name,
        allow_destructive,
        force_overwrite,
        &models,
        &apps,
        time::OffsetDateTime::now_utc(),
        Some(pk_flip_option),
    )
}

/// Shared compose body — separated from [`compose_cmd`] so tests can
/// drive it with explicit `models` and `apps` (the production entry
/// point sources both from `inventory::iter` and `AppRegistry::all`,
/// which are global state and thus not directly addressable from a
/// unit test).
///
/// Acquires the workspace lock, walks the on-disk migration tree to
/// recover orphaned snapshots (Codex B-1 — renamed-from buckets), and
/// invokes [`djogi::migrate::compose`].
// Compose has 8 inputs because it sits at the bridge between
// CLI flag parsing (workspace / name / flags / clock) and the
// engine (`models` / `apps` / `pk_flip_join_table_option`).
// Folding these into a struct would push the same fields onto
// the caller; the CLI tests already pass them positionally and
// a struct-based refactor would be churn for no clarity gain.
#[allow(clippy::too_many_arguments)]
fn compose_with_inputs(
    workspace: &Path,
    name: &str,
    allow_destructive: bool,
    force_overwrite: bool,
    models: &std::collections::BTreeMap<
        djogi::migrate::projection::BucketKey,
        djogi::migrate::AppliedSchema,
    >,
    apps: &[AppLifecycle],
    now: time::OffsetDateTime,
    pk_flip_join_table_option: Option<djogi::migrate::PkFlipJoinTableOption>,
) -> ExitCode {
    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations compose: failed to acquire workspace lock: {e}");
            return ExitCode::from(1);
        }
    };

    // Read snapshots from disk. Codex B-1: the bucket set we load is
    // the UNION of (a) every bucket the current projection knows
    // about and (b) every on-disk bucket that has a snapshot file.
    // Without (b) a renamed-from app's old snapshot is missed
    // entirely (the new app's `BucketKey` differs and the differ
    // never sees the old schema, breaking compose's rename + drop +
    // move emission).
    let mut bucket_set: std::collections::BTreeSet<djogi::migrate::projection::BucketKey> =
        models.keys().cloned().collect();
    for bucket in discover_snapshot_buckets_on_disk(workspace) {
        bucket_set.insert(bucket);
    }

    let mut snapshots: std::collections::BTreeMap<_, _> = std::collections::BTreeMap::new();
    for bucket in &bucket_set {
        let path = djogi::migrate::snapshot_path(workspace, bucket);
        match djogi::migrate::load_snapshot(&path) {
            Ok(s) => {
                snapshots.insert(bucket.clone(), s);
            }
            Err(djogi::migrate::SnapshotError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                // Fresh app — no prior snapshot.
            }
            Err(e) => {
                eprintln!(
                    "djogi migrations compose: snapshot load failed at {}: {e}",
                    path.display()
                );
                return ExitCode::from(1);
            }
        }
    }

    let req = ComposeRequest {
        workspace_root: workspace,
        models,
        snapshots: &snapshots,
        apps,
        name,
        allow_destructive,
        force_overwrite,
        now,
        _guard: &guard,
        pk_flip_join_table_option,
        // Production: always run Phase 0 auto-emit. The flag is a
        // test-only escape hatch for unit tests that exercise
        // compose's lower-level write/rollback machinery in
        // isolation; the CLI / production path always goes through
        // the full bootstrap flow.
        skip_phase_zero_auto_emit: false,
    };
    match compose(req) {
        Ok(report) => {
            // Track 0: surface auto-emitted Phase 0 bootstraps before
            // the regular composed buckets so the operator sees the
            // bootstrap context before the per-bucket changes.
            for emit in &report.emitted_phase_zero {
                let ext_summary = if emit.extensions.is_empty() {
                    "no extensions".to_string()
                } else {
                    format!(
                        "extensions: {}",
                        emit.extensions
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                println!(
                    "auto-emitted Phase 0 bootstrap: {database}/_global_ ({ext_summary})",
                    database = emit.database,
                );
            }
            for cb in &report.composed_buckets {
                println!(
                    "composed {database}/{app}: {version} ({classification:?})",
                    database = cb.bucket.database,
                    app = if cb.bucket.app.is_empty() {
                        "_global_"
                    } else {
                        cb.bucket.app.as_str()
                    },
                    version = cb.version,
                    classification = cb.classification,
                );
            }
            ExitCode::from(0)
        }
        Err(ComposeError::NothingToCompose) => {
            println!("nothing to compose — model state matches snapshot for every bucket");
            // Per the v3 §3 inline-decisions: nothing-to-compose is
            // not an error. The status command is the one that
            // signals out-of-sync state via exit code.
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("djogi migrations compose: {e}");
            ExitCode::from(1)
        }
    }
}

/// `djogi migrations status` entry point.
///
/// Read-only — does not acquire the workspace lock. Reads the
/// migration ledger from the active database via
/// [`djogi::context::DjogiContext`].
pub fn status_cmd(workspace: Option<PathBuf>) -> ExitCode {
    let workspace = resolve_workspace(workspace);

    // Build a tokio runtime so we can drive the async ledger query.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations status: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = runtime.block_on(async { run_status(&workspace).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`status_cmd`]. Returns the desired exit code.
///
/// Codex A-1: the resolved `workspace` path now feeds
/// [`djogi::config::DjogiConfig::load_from_workspace`] so a
/// `--workspace /custom/path` actually reads `/<custom>/Djogi.toml`
/// instead of always picking up the cwd's config. Production callers
/// running from inside the project root (the typical case) get the
/// previous behaviour for free — `resolve_workspace(None)` returns
/// `cwd`.
async fn run_status(workspace: &Path) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations status: config load: {e}");
            return 1;
        }
    };

    let mut ctx = match build_status_context(&config.database.url).await {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("djogi migrations status: pool: {e}");
            return 1;
        }
    };

    let rows = match djogi::migrate::select_all_ledger_rows(&mut ctx).await {
        Ok(rows) => rows,
        Err(e) => {
            // A missing ledger table is treated as "no migrations
            // applied" — print the empty state and exit 0.
            if e.to_string().contains("djogi_schema_migrations") {
                println!("No migrations recorded.");
                return 0;
            }
            eprintln!("djogi migrations status: ledger read: {e}");
            return 1;
        }
    };

    let registered: Vec<String> = AppRegistry::all()
        .iter()
        .map(|d| d.label.to_string())
        .collect();
    let report = djogi::migrate::render_status(&rows, &registered);
    for line in &report.lines {
        println!("{line}");
    }
    report.exit_code
}

/// Build a [`djogi::context::DjogiContext`] from a connection URL —
/// light wrapper so the status path stays readable. Connects via
/// `DjogiPool::connect` (the public connection-string entry point)
/// then hands off via the public `DjogiContext::from_pool` API.
async fn build_status_context(url: &str) -> Result<djogi::context::DjogiContext, String> {
    let pool = djogi::pg::pool::DjogiPool::connect(url)
        .await
        .map_err(|e| e.to_string())?;
    Ok(djogi::context::DjogiContext::from_pool(pool))
}

/// `djogi migrations attune` entry point.
///
/// Mode selection (per CLI flags):
///
/// | `--record-ledger` | `--squash` | resolved mode |
/// |-----------|-----------|---------------|
/// | false | false | [`AttuneMode::DiffOnly`] (read-only diff) |
/// | true  | false | [`AttuneMode::Record`] |
/// | false | true  | [`AttuneMode::Squash { from, publish, app }`] |
/// | true  | true  | rejected by clap (`conflicts_with`) |
///
/// Argument semantics:
/// - `target` is an optional positional Git target (commit / tag /
///   branch). When supplied, attune resolves it (local first, fetch
///   on miss) before any DB / disk mutation.
/// - `apply` gates DB / disk mutation. Without it, every mode is a
///   dry-run.
/// - `record` controls the parent repo's recorded submodule pointer
///   (separate from `record_ledger`, which controls the
///   `djogi_schema_migrations` ledger inserts).
///
/// `--squash` requires `--from <ver>`; an absent `from` while
/// `--squash` is set surfaces as a CLI error before any work happens.
// Codex umbrella U-1: the CLI dispatch carries 11 inputs because the
// attune surface is the broadest in the migrations CLI — target
// resolution + dry-run + record-ledger + record-pointer + squash +
// publish all live on the same command. Folding them into a struct
// would push the same fields onto the caller; the dispatch above
// already passes them positionally and a struct refactor would be
// churn for no clarity gain.
#[allow(clippy::too_many_arguments)]
pub fn attune_cmd(
    target: Option<&str>,
    apply: bool,
    record: bool,
    record_ledger: bool,
    record_reason: &str,
    squash: bool,
    from: Option<&str>,
    publish: bool,
    app: Option<&str>,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let mode = match (record_ledger, squash) {
        (false, false) => AttuneMode::DiffOnly,
        (true, false) => AttuneMode::Record {
            reason: record_reason.to_string(),
        },
        (false, true) => match from {
            Some(v) if !v.is_empty() => AttuneMode::Squash {
                from: v.to_string(),
                publish,
                app: app.filter(|s| !s.is_empty()).map(|s| s.to_string()),
            },
            _ => {
                eprintln!(
                    "djogi migrations attune --squash requires --from <version> (e.g. \
                     `--from V20260101000000__init`)"
                );
                return ExitCode::from(2);
            }
        },
        (true, true) => {
            // Already rejected by clap's `conflicts_with`; this branch
            // is defensive in case the flag is added programmatically.
            eprintln!(
                "djogi migrations attune: --record-ledger and --squash are mutually exclusive"
            );
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations attune: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let target_owned = target.map(str::to_string);
    let exit =
        runtime.block_on(async { run_attune(&workspace, mode, target_owned, apply, record).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`attune_cmd`]. Loads config, builds the context,
/// acquires the workspace lock, invokes the library entry point.
async fn run_attune(
    workspace: &Path,
    mode: AttuneMode,
    target: Option<String>,
    apply: bool,
    record: bool,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations attune: config load: {e}");
            return 1;
        }
    };

    let mut ctx = match build_status_context(&config.database.url).await {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("djogi migrations attune: pool: {e}");
            return 1;
        }
    };

    // All three modes acquire the workspace lock per the v3 file-lock
    // contract — even DiffOnly takes the lock so a concurrent compose
    // / apply cannot mutate the tree mid-scan.
    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations attune: failed to acquire workspace lock: {e}");
            return 1;
        }
    };

    let req = AttuneRequest {
        workspace_root: workspace,
        database_url: &config.database.url,
        profile: &config.profile,
        // Codex umbrella U-2: thread `[database].dev_mode` to the
        // squash gate. Read-only modes (`DiffOnly`, `Record`) ignore
        // it; `Squash` mode refuses unless this is `true`.
        dev_mode: config.database.dev_mode,
        // Codex umbrella U-1: the operator-supplied target + the
        // `--apply` / `--record` gates flow through to the library
        // entry point. The library owns the resolution + parent-pointer
        // update; the CLI is just plumbing.
        target: target.as_deref(),
        apply,
        record,
        mode,
        _guard: &guard,
    };
    match attune(&mut ctx, req).await {
        Ok(report) => {
            if report.entries.is_empty() {
                println!("attune: no drift");
            } else {
                for entry in &report.entries {
                    let app_display = if entry.bucket.app.is_empty() {
                        "_global_"
                    } else {
                        entry.bucket.app.as_str()
                    };
                    println!(
                        "  {kind:<10}  {database}/{app}  {version}",
                        kind = entry.kind.as_str(),
                        database = entry.bucket.database,
                        app = app_display,
                        version = entry.version,
                    );
                }
            }
            // Surface structured diagnostics — today this carries the
            // B-3 LedgerTableMissing notice when DiffOnly runs on a
            // fresh database.
            for diag in &report.diagnostics {
                println!("  diagnostic: {diag}");
            }
            if let Some(sha) = &report.resolved_target {
                println!("resolved target: {sha}");
            }
            if let Some(squashed) = &report.squashed_to {
                println!("squashed to: {squashed}");
            }
            if report.published {
                println!("published to remote");
            }
            if report.parent_pointer_updated {
                println!("parent submodule pointer updated");
            }
            0
        }
        Err(e) => {
            eprintln!("djogi migrations attune: {e}");
            attune_error_exit_code(&e)
        }
    }
}

/// Map an [`AttuneError`] variant onto the documented exit-code
/// matrix (`docs/spec/configuration.md` §14):
///
/// - Refusal variants → exit code `2` ("operator must intervene;
///   nothing happened"). Today every refusal flows through
///   [`AttuneError::Refused`]; the localhost gate, the dev-profile
///   gate, the missing-version refusal, and the ambiguous-version
///   refusal are all reachable through that variant.
/// - Runtime variants → exit code `1` ("we tried; something broke" —
///   filesystem scan, ledger query, SQL read/write/delete, git
///   publish). CI may safely retry these.
///
/// Pulled out as a free function so unit tests can pin every variant
/// without spinning a Tokio runtime. Operators rely on the 1-vs-2
/// distinction to tell "refused before any side effect" from "ran and
/// failed mid-flight".
fn attune_error_exit_code(err: &AttuneError) -> i32 {
    match err {
        AttuneError::Refused(_) => 2,
        AttuneError::FilesystemScanFailed { .. }
        | AttuneError::LedgerQueryFailed { .. }
        | AttuneError::SqlReadFailed { .. }
        | AttuneError::SqlWriteFailed { .. }
        | AttuneError::SqlDeleteFailed { .. }
        | AttuneError::GitPublishFailed { .. }
        | AttuneError::GitTargetResolveFailed { .. }
        | AttuneError::GitFetchFailed { .. }
        | AttuneError::GitUpdateSubmodulePointerFailed { .. } => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("djogi-cli-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Codex B-1 — the CLI's bucket-discovery walk must include
    /// directories that exist on disk but are absent from the current
    /// model inventory (the renamed-from case).
    #[test]
    fn b1_discover_snapshot_buckets_picks_up_renamed_from_app() {
        let work = temp_workspace("b1_discover");
        // Lay down a `migrations/main/billing/schema_snapshot.json`
        // — the OLD app's snapshot. The current model inventory
        // would NOT have this bucket because the app moved to
        // `invoicing` via `#[app(renamed_from = "billing")]`.
        let billing_dir = work.join("migrations/main/billing");
        fs::create_dir_all(&billing_dir).unwrap();
        fs::write(billing_dir.join("schema_snapshot.json"), "{}").unwrap();
        // A second bucket — the global one for the same database —
        // exists too. Exercise the multi-bucket walk.
        let global_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("schema_snapshot.json"), "{}").unwrap();
        // A third on-disk directory WITHOUT a snapshot file — must
        // not be reported (we only union buckets that actually
        // shipped a snapshot).
        let no_snap_dir = work.join("migrations/main/empty_app");
        fs::create_dir_all(&no_snap_dir).unwrap();

        let buckets = discover_snapshot_buckets_on_disk(&work);
        let labels: std::collections::BTreeSet<&str> =
            buckets.iter().map(|b| b.app.as_str()).collect();
        assert!(
            labels.contains("billing"),
            "must include the renamed-from bucket: {labels:?}"
        );
        assert!(
            labels.contains(""),
            "must include the global bucket: {labels:?}"
        );
        assert!(
            !labels.contains("empty_app"),
            "must not include directories without a snapshot: {labels:?}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex A-1 — the resolved workspace flows into config loading.
    /// `load_from_workspace` must read `<workspace>/Djogi.toml` not
    /// the cwd's. We assert that by writing a custom config with a
    /// distinctive `database.url` and confirming the loader sees it.
    #[test]
    fn a1_load_from_workspace_reads_path_specific_djogi_toml() {
        let work = temp_workspace("a1_workspace_config");
        let toml = "[database]\nurl = \"postgres://discovered-by-workspace-flag/test\"\n\
                    max_connections = 1\ndev_mode = false\n\
                    [server]\nhost = \"127.0.0.1\"\nport = 1234\n";
        fs::write(work.join("Djogi.toml"), toml).unwrap();
        // Save and clear DATABASE_URL so the env override doesn't
        // mask the file value during this test.
        let prior = std::env::var("DATABASE_URL").ok();
        // SAFETY: tests run with --test-threads=1 per the project's
        // pre-commit policy, so concurrent env mutation is not a
        // concern in this configuration.
        unsafe { std::env::remove_var("DATABASE_URL") };
        let config = djogi::config::DjogiConfig::load_from_workspace(&work).expect("load");
        assert_eq!(
            config.database.url,
            "postgres://discovered-by-workspace-flag/test"
        );
        assert_eq!(config.server.port, 1234);
        if let Some(v) = prior {
            unsafe { std::env::set_var("DATABASE_URL", v) };
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 A-1 — env override precedence. A `DATABASE_URL`
    /// in the environment must beat any value in
    /// `<workspace>/Djogi.toml`, matching the security contract that
    /// secrets only live in env vars.
    #[test]
    fn a1_round2_env_override_beats_workspace_toml() {
        let work = temp_workspace("a1r2_env_override");
        let toml = "[database]\nurl = \"postgres://from-toml/test\"\n\
                    max_connections = 1\ndev_mode = false\n\
                    [server]\nhost = \"127.0.0.1\"\nport = 1234\n";
        fs::write(work.join("Djogi.toml"), toml).unwrap();
        let prior = std::env::var("DATABASE_URL").ok();
        // SAFETY: --test-threads=1; no concurrent env mutation.
        unsafe { std::env::set_var("DATABASE_URL", "postgres://from-env/test") };
        let config = djogi::config::DjogiConfig::load_from_workspace(&work).expect("load");
        assert_eq!(
            config.database.url, "postgres://from-env/test",
            "env DATABASE_URL must win over workspace Djogi.toml"
        );
        match prior {
            Some(v) => unsafe { std::env::set_var("DATABASE_URL", v) },
            None => unsafe { std::env::remove_var("DATABASE_URL") },
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-1 — `compose_with_inputs` must consume the
    /// disk-discovered buckets, not just the inventory's. We set up a
    /// `migrations/main/billing/schema_snapshot.json` with a `widgets`
    /// table, pass an EMPTY models map (simulating "billing app was
    /// removed from the workspace"), set `allow_destructive = true`,
    /// and assert the resulting up SQL contains `DROP TABLE
    /// "widgets"`. If the disk-walk regressed and `compose_with_inputs`
    /// only loaded snapshots for inventory-known buckets, the differ
    /// would never see billing's snapshot and the compose would exit
    /// `NothingToCompose` (no DROP, no SQL written).
    ///
    /// This is the end-to-end pinning B-1 round-1 missed.
    #[test]
    fn b1_round2_compose_consumes_discovered_orphan_snapshot() {
        use djogi::migrate::projection::BucketKey;
        use djogi::migrate::schema::{
            ColumnSchema, PkKindSchema, PrimaryKeySchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
        };
        use djogi::migrate::{AppliedSchema, save_snapshot, snapshot_path};
        use std::collections::BTreeMap;

        let work = temp_workspace("b1r2_compose_uses_discovery");

        // Build a billing-bucket snapshot with one `widgets` table
        // and write it to disk under `migrations/main/billing/`.
        let billing_bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let mut billing_snap = AppliedSchema {
            djogi_version: env!("CARGO_PKG_VERSION").to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["billing".to_string()],
        };
        billing_snap.models.insert(
            "widgets".to_string(),
            TableSchema {
                app: Some("billing".to_string()),
                columns: vec![ColumnSchema {
                    check: None,
                    default_sql: Some("heerid_next_desc()".to_string()),
                    foreign_key: None,
                    generated: None,
                    identity: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "id".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                }],
                exclusion_constraints: Vec::new(),
                fts: None,
                is_through: false,
                moved_from_app: None,
                partition: None,
                primary_key: PrimaryKeySchema {
                    columns: vec!["id".to_string()],
                    kind: PkKindSchema::HeerIdRecencyBiased,
                },
                rationale: None,
                renamed_from: None,
                rls_enabled: false,
                table: "widgets".to_string(),
                tenant_key: None,
            },
        );
        let snap_path = snapshot_path(&work, &billing_bucket);
        save_snapshot(&billing_snap, &snap_path).expect("write snapshot");

        // EMPTY models — simulates the billing crate having been
        // removed from the workspace. Without the disk-walk this
        // bucket would never reach the differ.
        let empty_models: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
        let now = time::OffsetDateTime::from_unix_timestamp(1_745_549_523).unwrap();

        let exit = compose_with_inputs(
            &work,
            "drop billing remnant",
            true,  // allow_destructive — billing's snapshot will produce DROP ops
            false, // force_overwrite
            &empty_models,
            &[],
            now,
            None, // pk_flip_join_table_option — no flip in this test
        );
        assert_eq!(exit, ExitCode::from(0), "compose must succeed");

        // The composed up SQL must carry DROP TABLE for billing's
        // widgets — that is the whole point. Find the file and check.
        let billing_dir = djogi::migrate::bucket_dir(&work, &billing_bucket);
        let mut up_path: Option<PathBuf> = None;
        for entry in fs::read_dir(&billing_dir).unwrap().flatten() {
            let n = entry.file_name().to_string_lossy().to_string();
            // Up file pattern: starts with "V", ends with ".sql", does
            // NOT contain ".down.".
            if n.starts_with('V') && n.ends_with(".sql") && !n.contains(".down.") {
                up_path = Some(entry.path());
                break;
            }
        }
        let up_path = up_path.expect("compose must have written an up SQL file");
        let up_sql = fs::read_to_string(&up_path).unwrap();
        assert!(
            up_sql.contains("DROP TABLE \"widgets\""),
            "compose must have seen the disk snapshot and emitted DROP TABLE — \
             this proves discover_snapshot_buckets_on_disk reached the differ. \
             SQL: {up_sql}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 A-1 — `status_cmd` invokes its tokio runtime and
    /// fails fast on a malformed `Djogi.toml`. We don't need a live
    /// Postgres for this assertion — the test is that the workspace
    /// path is threaded through the loader and TOML errors surface
    /// promptly. (The earlier `a1_load_from_workspace_reads_path_specific_djogi_toml`
    /// covers the well-formed case; this is the malformed-input
    /// path-threading proof.)
    #[test]
    fn a1_round2_status_cmd_threads_workspace_to_config() {
        let work = temp_workspace("a1r2_status_workspace");
        // Write a deliberately malformed TOML so config load fails.
        // If the workspace path wasn't threaded, status_cmd would
        // try the cwd's Djogi.toml (typically absent) and silently
        // fall through to defaults, giving a different error code.
        fs::write(work.join("Djogi.toml"), "this is = not = valid toml ===").unwrap();
        let exit = status_cmd(Some(work.clone()));
        assert_eq!(
            exit,
            ExitCode::from(1),
            "malformed workspace Djogi.toml must surface as config load error"
        );
        let _ = fs::remove_dir_all(&work);
    }

    // ── Codex umbrella U-3: AttuneError → exit code matrix ───────────────

    /// Every `AttuneError::Refused(_)` variant must map to exit code `2`
    /// per `docs/spec/configuration.md` §14. The pre-fix implementation
    /// flattened every error to `1`, so an operator running attune in CI
    /// could not distinguish "policy gate refused before any side effect"
    /// from "ran half a step and failed mid-flight". Codex umbrella U-3
    /// flagged this as a blocker.
    #[test]
    fn u3_attune_refusal_variants_map_to_exit_code_two() {
        use djogi::migrate::AttuneRefusal;
        let cases = [
            AttuneError::Refused(AttuneRefusal::SquashNotLocalhost {
                database_url: "postgres://prod.example.com/main".to_string(),
            }),
            AttuneError::Refused(AttuneRefusal::SquashNotDevProfile {
                profile: "production".to_string(),
            }),
            // Codex umbrella U-2 — dev_mode and DJOGI_ENV gates added
            // in the same fixup chain. Both are `AttuneError::Refused(_)`
            // so they share the exit-code-2 mapping.
            AttuneError::Refused(AttuneRefusal::SquashDevModeOff),
            AttuneError::Refused(AttuneRefusal::SquashEnvIsProduction {
                env_value: "production".to_string(),
            }),
            AttuneError::Refused(AttuneRefusal::SquashFromVersionNotFound {
                version: "V20260101000000__missing".to_string(),
            }),
            AttuneError::Refused(AttuneRefusal::SquashFromVersionAmbiguous {
                version: "V20260101000000__shared".to_string(),
                buckets: vec!["main/users".to_string(), "main/billing".to_string()],
            }),
        ];
        for err in &cases {
            assert_eq!(
                attune_error_exit_code(err),
                2,
                "refusal variant must map to exit 2: {err}"
            );
        }
    }

    /// Every runtime `AttuneError` variant must map to exit code `1`
    /// per `docs/spec/configuration.md` §14. CI may safely retry runtime
    /// failures; a refusal (exit `2`) signals "operator must intervene"
    /// and retrying without operator action would just refuse again.
    #[test]
    fn u3_attune_runtime_variants_map_to_exit_code_one() {
        let cases = [
            AttuneError::FilesystemScanFailed {
                source: std::io::Error::other("disk full"),
            },
            AttuneError::SqlReadFailed {
                path: PathBuf::from("/tmp/x.sql"),
                source: std::io::Error::other("permission denied"),
            },
            AttuneError::SqlWriteFailed {
                path: PathBuf::from("/tmp/x.sql"),
                source: std::io::Error::other("read-only fs"),
            },
            AttuneError::SqlDeleteFailed {
                path: PathBuf::from("/tmp/x.sql"),
                source: std::io::Error::other("not found"),
            },
            AttuneError::GitPublishFailed {
                stderr: "fatal: refusing to push".to_string(),
                status_code: Some(128),
            },
        ];
        for err in &cases {
            assert_eq!(
                attune_error_exit_code(err),
                1,
                "runtime variant must map to exit 1: {err}"
            );
        }
    }
}
