//! `djogi migrations` subcommand glue — Phase 7 T6.
//!
//! Two leaves: `compose` and `status`. Both flow through the public
//! `djogi::migrate` API. Compose acquires the workspace file lock for
//! the duration of the call; status is read-only and does not.
//!
//! The CLI surface here is intentionally thin — all the real logic
//! lives in the library so integration tests can exercise it without
//! spawning subprocesses.

use std::path::PathBuf;
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

/// `djogi migrations compose` entry point.
pub fn compose_cmd(name: &str, allow_destructive: bool, workspace: Option<PathBuf>) -> ExitCode {
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

    // Read snapshots from disk for every bucket the projection knows
    // about. Buckets without an existing snapshot stay absent from
    // the map; compose treats them as fresh apps.
    let mut snapshots: std::collections::BTreeMap<_, _> = std::collections::BTreeMap::new();
    for bucket in models.keys() {
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
    let _workspace = resolve_workspace(workspace);

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

    let exit = runtime.block_on(async { run_status().await });
    ExitCode::from(exit as u8)
}

/// Async body of [`status_cmd`]. Returns the desired exit code.
async fn run_status() -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load() {
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
    // The `_workspace` variable above is currently unused — the
    // status path only reads the ledger from the database. We accept
    // the flag because `compose` accepts it and CLI ergonomics favour
    // a consistent shape; the workspace path will become load-bearing
    // when status grows the optional snapshot ↔ ledger cross-check
    // (T8 follow-up).
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
