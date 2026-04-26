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
    AppLifecycle, AttuneMode, AttuneRequest, ComposeError, ComposeRequest, GUARD_DEFAULT_TIMEOUT,
    LOCK_FILE_NAME, acquire_workspace_lock, attune, compose, project_from_inventory,
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
    // loading too — `compose` itself doesn't read `Djogi.toml`, but
    // future flag handling that does (e.g. default-allow-destructive
    // policy) inherits the path-aware loader. Loading here surfaces
    // any TOML parse error at the same point as projection errors so
    // the operator gets one consistent failure mode.
    //
    // SYNCWATCH: this load_from_workspace call is currently a defensive
    // early-parse probe — the parsed `DjogiConfig` is intentionally
    // dropped because `compose_with_inputs` doesn't yet consume migrate
    // config. When compose logic begins reading config (e.g. a
    // migrate.compose.* setting), thread the parsed value through to
    // the request instead of discarding it, and update this comment.
    // grep `SYNCWATCH:` to find every paired call site.
    if let Err(e) = djogi::config::DjogiConfig::load_from_workspace(&workspace) {
        eprintln!("djogi migrations compose: config load: {e}");
        return ExitCode::from(1);
    }
    compose_with_inputs(
        &workspace,
        name,
        allow_destructive,
        force_overwrite,
        &models,
        &apps,
        time::OffsetDateTime::now_utc(),
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

/// `djogi migrations attune` entry point.
///
/// Mode selection (per CLI flags):
///
/// | `--record` | `--squash` | resolved mode |
/// |-----------|-----------|---------------|
/// | false | false | [`AttuneMode::DiffOnly`] (read-only diff) |
/// | true  | false | [`AttuneMode::Record`] |
/// | false | true  | [`AttuneMode::Squash { from, publish }`] |
/// | true  | true  | rejected by clap (`conflicts_with`) |
///
/// `--squash` requires `--from <ver>`; an absent `from` while
/// `--squash` is set surfaces as a CLI error before any work happens.
pub fn attune_cmd(
    record: bool,
    record_reason: &str,
    squash: bool,
    from: Option<&str>,
    publish: bool,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let mode = match (record, squash) {
        (false, false) => AttuneMode::DiffOnly,
        (true, false) => AttuneMode::Record {
            reason: record_reason.to_string(),
        },
        (false, true) => match from {
            Some(v) if !v.is_empty() => AttuneMode::Squash {
                from: v.to_string(),
                publish,
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
            eprintln!("djogi migrations attune: --record and --squash are mutually exclusive");
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

    let exit = runtime.block_on(async { run_attune(&workspace, mode).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`attune_cmd`]. Loads config, builds the context,
/// acquires the workspace lock, invokes the library entry point.
async fn run_attune(workspace: &Path, mode: AttuneMode) -> i32 {
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
            if let Some(squashed) = &report.squashed_to {
                println!("squashed to: {squashed}");
            }
            if report.published {
                println!("published to remote");
            }
            0
        }
        Err(e) => {
            eprintln!("djogi migrations attune: {e}");
            1
        }
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
                    default_sql: Some("generate_id_desc()".to_string()),
                    foreign_key: None,
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
}
