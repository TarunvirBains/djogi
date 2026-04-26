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
    AppLifecycle, ComposeError, ComposeRequest, GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME,
    acquire_workspace_lock, compose, project_from_inventory,
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
/// Per Codex B-1: compose's `snapshots` map must include the OLD
/// bucket of any renamed app — and that bucket is guaranteed to be
/// absent from the current `models` inventory because the
/// `#[app(renamed_from = "old")]` annotation lives on the NEW app.
/// Walking disk directly recovers those orphaned snapshots so the
/// differ sees both sides of a rename.
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
    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations compose: failed to acquire workspace lock: {e}");
            return ExitCode::from(1);
        }
    };

    let models = match project_from_inventory() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("djogi migrations compose: projection error: {e}");
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
    for bucket in discover_snapshot_buckets_on_disk(&workspace) {
        bucket_set.insert(bucket);
    }

    let mut snapshots: std::collections::BTreeMap<_, _> = std::collections::BTreeMap::new();
    for bucket in &bucket_set {
        let path = djogi::migrate::snapshot_path(&workspace, bucket);
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

    let apps: Vec<AppLifecycle> = AppRegistry::all()
        .iter()
        .map(|d| AppLifecycle {
            label: d.label.to_string(),
            database: d.database.to_string(),
            renamed_from: d.renamed_from.map(str::to_string),
            tombstone: d.tombstone,
        })
        .collect();

    let now = time::OffsetDateTime::now_utc();
    let req = ComposeRequest {
        workspace_root: &workspace,
        models: &models,
        snapshots: &snapshots,
        apps: &apps,
        name,
        allow_destructive,
        force_overwrite,
        now,
        _guard: &guard,
    };
    match compose(req) {
        Ok(report) => {
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
}
