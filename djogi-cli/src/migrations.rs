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
    AppLifecycle, AttuneError, AttuneMode, AttuneRequest, BucketKey, ComposeError, ComposeRequest,
    GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME, PendingPlan, RunnerCtx, RunnerError, SnapshotError,
    VerifyReport, VerifySeverity, acquire_workspace_lock, apply_plan, attune, compose,
    fake_apply_plan, load_snapshot, project_from_inventory, snapshot_path,
};

// Re-export for the apply command's ledger state machine.
use djogi::migrate::LedgerStatus;

// ── Replay plan deserialization ──────────────────────────────────────────

/// Local mirror of `StoredReplayPlan` (pub(crate) in the library).
///
/// The committed replay plan JSON written by `compose` at
/// `migrations/<database>/<app>/<version>.plan.json`. This struct
/// allows the CLI to parse it and construct a proper [`MigrationPlan`]
/// with correct segment structure and checksums.
#[derive(Debug, Clone, serde::Deserialize)]
struct CliReplayPlan {
    format_version: String,
    checksum_up: String,
    checksum_down: Option<String>,
    classification: CliClassification,
    segments: Vec<CliReplaySegment>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CliClassification {
    NoOp,
    Additive,
    Reversible,
    Destructive,
    Lossy,
    Unsupported {
        reason: String,
    },
    PkTypeFlip {
        co_destructive: bool,
        co_lossy: bool,
    },
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CliReplaySegment {
    kind: CliSegmentKind,
    statements: Vec<CliReplayStatement>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum CliSegmentKind {
    Transactional,
    NonTransactional,
    MetadataOnly,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CliReplayStatement {
    label: String,
    up: String,
}

/// Expected format version for the committed replay plan JSON.
const CLI_REPLAY_PLAN_FORMAT_VERSION: &str = "1";

/// Load the committed replay plan from disk and convert to a
/// [`djogi::migrate::MigrationPlan`]. Returns `(plan, checksum_up, checksum_down)`.
///
/// Falls back to reading the up/down SQL files and constructing a
/// single-segment transactional plan when the replay plan JSON is
/// absent or invalid. This mirrors the reset.rs fallback path.
fn load_replay_plan_from_disk(
    workspace: &Path,
    bucket: &djogi::migrate::BucketKey,
    version: &str,
    pending_checksum_up: &str,
    pending_checksum_down: Option<&str>,
) -> Result<(djogi::migrate::MigrationPlan, String, Option<String>), ApplyReplayPlanError> {
    // Try to load the committed replay plan JSON first.
    let bucket_dir = djogi::migrate::bucket_dir(workspace, bucket);
    let replay_plan_path = bucket_dir.join(format!("{version}.plan.json"));

    if let Ok(bytes) = std::fs::read(&replay_plan_path) {
        let stored: CliReplayPlan = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(e) => {
                return Err(ApplyReplayPlanError::Parse {
                    path: replay_plan_path.clone(),
                    source: e.to_string(),
                });
            }
        };

        if stored.format_version != CLI_REPLAY_PLAN_FORMAT_VERSION {
            return Err(ApplyReplayPlanError::FormatVersion {
                found: stored.format_version,
                path: replay_plan_path.clone(),
            });
        }

        // Verify checksums match the pending plan.
        if stored.checksum_up != pending_checksum_up
            || stored.checksum_down.as_deref() != pending_checksum_down
        {
            return Err(ApplyReplayPlanError::ChecksumMismatch);
        }

        let plan = djogi::migrate::MigrationPlan {
            bucket: bucket.clone(),
            classification: stored.classification.into(),
            segments: stored
                .segments
                .into_iter()
                .map(|seg| djogi::migrate::Segment {
                    kind: seg.kind.into(),
                    statements: seg
                        .statements
                        .into_iter()
                        .map(|stmt| djogi::migrate::OperationSql {
                            label: stmt.label,
                            up: stmt.up,
                            down: String::new(),
                            lossy: None,
                        })
                        .collect(),
                })
                .collect(),
        };

        return Ok((plan, stored.checksum_up, stored.checksum_down));
    }

    // Fallback: read SQL files and construct single-segment plan.
    let up_filename = djogi::migrate::up_filename(version);
    let down_filename = djogi::migrate::down_filename(version);
    let up_path = bucket_dir.join(&up_filename);
    let down_path = bucket_dir.join(&down_filename);

    let up_sql = std::fs::read_to_string(&up_path).map_err(|e| ApplyReplayPlanError::SqlRead {
        path: up_path.clone(),
        source: e.to_string(),
    })?;

    let down_sql = match std::fs::read_to_string(&down_path) {
        Ok(sql) => sql,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ApplyReplayPlanError::SqlRead {
                path: down_path.clone(),
                source: e.to_string(),
            });
        }
    };

    // Compute checksum for the single-segment fallback. The runner
    // recomputes from the plan's SQL fragments and verifies against
    // what we provide in RunnerCtx, so they must match.
    let computed_checksum_up = djogi::migrate::compute_checksum([&up_sql]);

    // Build a single-transactional-segment plan. This is correct for
    // most migrations — only CONCURRENTLY indexes require non-tx
    // segments, and those always have a replay plan JSON.
    let plan = djogi::migrate::MigrationPlan {
        bucket: bucket.clone(),
        classification: djogi::migrate::Classification::Additive,
        segments: vec![djogi::migrate::Segment {
            kind: djogi::migrate::SegmentKind::Transactional,
            statements: vec![djogi::migrate::OperationSql {
                label: format!("replay {version}"),
                up: up_sql,
                down: down_sql,
                lossy: None,
            }],
        }],
    };

    Ok((plan, computed_checksum_up, None))
}

/// Errors from [`load_replay_plan_from_disk`].
#[derive(Debug)]
enum ApplyReplayPlanError {
    Parse { path: PathBuf, source: String },
    FormatVersion { found: String, path: PathBuf },
    ChecksumMismatch,
    SqlRead { path: PathBuf, source: String },
}

impl std::fmt::Display for ApplyReplayPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { path, source } => {
                write!(f, "parse replay plan {}: {source}", path.display())
            }
            Self::FormatVersion { found, path } => write!(
                f,
                "replay plan format version mismatch in {}: expected {}, found {}",
                path.display(),
                CLI_REPLAY_PLAN_FORMAT_VERSION,
                found
            ),
            Self::ChecksumMismatch => {
                write!(f, "checksum mismatch between pending JSON and replay plan")
            }
            Self::SqlRead { path, source } => {
                write!(f, "read SQL file {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ApplyReplayPlanError {}

// ── Type conversions from CLI-local types to library types ────────────────

impl From<CliSegmentKind> for djogi::migrate::SegmentKind {
    fn from(kind: CliSegmentKind) -> Self {
        match kind {
            CliSegmentKind::Transactional => Self::Transactional,
            CliSegmentKind::NonTransactional => Self::NonTransactional,
            CliSegmentKind::MetadataOnly => Self::MetadataOnly,
        }
    }
}

impl From<CliClassification> for djogi::migrate::Classification {
    fn from(classification: CliClassification) -> Self {
        match classification {
            CliClassification::NoOp => Self::NoOp,
            CliClassification::Additive => Self::Additive,
            CliClassification::Reversible => Self::Reversible,
            CliClassification::Destructive => Self::Destructive,
            CliClassification::Lossy => Self::Lossy,
            CliClassification::Unsupported { reason } => Self::Unsupported { reason },
            CliClassification::PkTypeFlip {
                co_destructive,
                co_lossy,
            } => Self::PkTypeFlip {
                co_destructive,
                co_lossy,
            },
        }
    }
}

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
    djogi::pg::preflight::check_postgres_version(&pool)
        .await
        .map_err(|e| format!("support boundary: {e}"))?;
    Ok(djogi::context::DjogiContext::from_pool(pool))
}

/// `djogi migrations apply` entry point.
///
/// Discovers pending JSON files under `target/djogi_pending/`, loads the
/// committed replay plan for each, and drives [`djogi::migrate::apply_plan`]
/// through the library runner with full crash recovery via the ledger state
/// machine.
pub fn apply_cmd(workspace: Option<PathBuf>, fake: bool, reason: Option<String>) -> ExitCode {
    let workspace = resolve_workspace(workspace);

    // Validate --fake / --reason pairing before doing any expensive work.
    let mode = if fake {
        match reason {
            Some(r) if !r.trim().is_empty() => FakeMode::Fake { reason: r },
            Some(_) => {
                eprintln!(
                    "djogi migrations apply --fake: --reason must not be empty; \
                     supply a non-empty reason why these migrations are being \
                     faked (e.g. 'schema pre-exists from prior tooling')"
                );
                return ExitCode::from(2);
            }
            None => {
                eprintln!(
                    "djogi migrations apply --fake: --reason is required; \
                     supply a reason why these migrations are being faked \
                     (e.g. 'schema pre-exists from prior tooling'). \
                     This is recorded in the ledger audit trail."
                );
                return ExitCode::from(2);
            }
        }
    } else {
        FakeMode::Real
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations apply: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = runtime.block_on(async { run_apply(&workspace, &mode).await });
    ExitCode::from(exit as u8)
}

/// Controls whether `apply_one_pending` executes SQL or records a
/// fake-apply row in the ledger.
#[derive(Debug, Clone)]
enum FakeMode {
    /// Execute DDL via `apply_plan`. Normal migration apply.
    Real,
    /// Skip DDL; record `status = 'faked'` via `fake_apply_plan`.
    Fake { reason: String },
}

/// Async body of [`apply_cmd`]. Returns the desired exit code.
async fn run_apply(workspace: &Path, mode: &FakeMode) -> i32 {
    use djogi::config::DjogiConfig;

    let action_verb = match mode {
        FakeMode::Real => "apply",
        FakeMode::Fake { .. } => "fake-apply",
    };
    let progress_verb = match mode {
        FakeMode::Real => "applying",
        FakeMode::Fake { .. } => "faking",
    };

    // 1. Load config.
    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: config load: {e}");
            return 2;
        }
    };

    // 2. Build pool and check PG version preflight.
    let pool = match djogi::pg::pool::DjogiPool::connect(&config.database.url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: pool connect: {e}");
            return 1;
        }
    };
    if let Err(e) = djogi::pg::preflight::check_postgres_version(&pool).await {
        crate::print_support_boundary_error("migrations apply", &e);
        return 2;
    }

    // 3. Discover pending JSONs.
    let pending_files = discover_pending_plans(workspace);
    if pending_files.is_empty() {
        println!("No pending migrations to {action_verb}.");
        return 0;
    }

    // 4. Acquire workspace lock.
    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: workspace lock: {e}");
            return 1;
        }
    };

    // 5. Build audit pool (optional — silently skipped if unavailable).
    let audit_pool = match djogi::migrate::resolve_audit_url(&config) {
        Ok(url) => djogi::migrate::build_audit_pool(&url).await.ok(),
        Err(_) => None,
    };

    // 6. Build context from pool (not pinned yet — apply_plan pins internally).
    let mut ctx = djogi::context::DjogiContext::from_pool(pool);

    // 7. Apply each pending migration in order.
    for (pending_path, bucket_database, app_label) in &pending_files {
        println!("  {progress_verb} {bucket_database}/{app_label}...");
        let result = apply_one_pending(
            &mut ctx,
            workspace,
            pending_path,
            bucket_database.clone(),
            app_label.clone(),
            &config,
            &guard,
            audit_pool.as_ref(),
            mode,
        )
        .await;

        match result {
            ApplyResult::Ok => match mode {
                FakeMode::Real => {
                    println!("Applied: {bucket_database}/{app_label}");
                }
                FakeMode::Fake { .. } => {
                    println!(
                        "  faked {bucket_database}/{app_label}: \
                             recorded in ledger with status = 'faked' (no SQL executed)"
                    );
                }
            },
            ApplyResult::Skipped(reason) => {
                println!("Skipped {bucket_database}/{app_label}: {reason}");
            }
            ApplyResult::Refused(reason) => {
                eprintln!(
                    "djogi migrations apply: refused {bucket_database}/{app_label}: {reason}"
                );
                return 2;
            }
            ApplyResult::RunnerError(e) => {
                eprintln!(
                    "djogi migrations apply: runner error on {bucket_database}/{app_label}: {e}"
                );
                return runner_error_exit_code(&e);
            }
        }
    }

    let summary_verb = match mode {
        FakeMode::Real => "applied",
        FakeMode::Fake { .. } => "faked",
    };
    println!("{summary_verb} {} migration(s).", pending_files.len());
    0
}

/// Outcome of applying a single pending migration.
#[derive(Debug)]
enum ApplyResult {
    /// Migration applied successfully.
    Ok,
    /// Migration skipped (already applied or no-op).
    Skipped(String),
    /// User-facing refusal — exit code 2.
    Refused(String),
    /// Runner error — exit code 1.
    RunnerError(RunnerError),
}

/// Scan `target/djogi_pending/` for pending JSON files.
///
/// Returns a list of `(path, database, app)` tuples sorted by file
/// name so the apply order is deterministic. Each path points to a
/// valid JSON file that was discovered on disk.
fn discover_pending_plans(workspace: &Path) -> Vec<(PathBuf, String, String)> {
    let pending_root = djogi::migrate::pending_root(workspace);
    let mut out = Vec::new();

    let Ok(db_entries) = std::fs::read_dir(&pending_root) else {
        return out;
    };

    for db_entry in db_entries.flatten() {
        let db_name = match db_entry.file_name().to_str().map(str::to_string) {
            Some(n) => n,
            None => continue,
        };
        if db_name.starts_with('.') {
            continue;
        }

        let db_dir = db_entry.path();
        if !db_dir.is_dir() {
            continue;
        }

        let Ok(app_entries) = std::fs::read_dir(&db_dir) else {
            continue;
        };

        for app_entry in app_entries.flatten() {
            let path = app_entry.path();
            if !path.is_file() {
                continue;
            }
            let filename = match path.file_name().and_then(|f| f.to_str()) {
                Some(f) => f,
                None => continue,
            };
            // Filter: must be a .json file, not the special _global_.json
            // pattern which is handled correctly by the naming function.
            if !filename.ends_with(".json") {
                continue;
            }
            // Extract app label from filename by stripping .json extension.
            // The pending JSON filename is `<app>.json` or `_global_.json`.
            let app_label = if let Some(stripped) = filename.strip_suffix(".json") {
                stripped.to_string()
            } else {
                continue;
            };

            out.push((path, db_name.clone(), app_label));
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Apply a single pending migration.
///
/// Loads the pending JSON to recover bucket and version, checks the
/// ledger state machine for crash recovery, loads the committed replay
/// plan (or falls back to a single-segment plan from the SQL file), and
/// drives [`djogi::migrate::apply_plan`].
///
/// Uses the bypass attribute because deleting failed ledger rows requires
/// raw SQL that is not exposed through the public typed API.
// apply_one_pending carries 9 arguments because it sits at the bridge
// between the CLI dispatch (workspace, path, bucket info) and the
// library runner (config, guard, audit pool, mode). Folding these into a
// struct would push the same fields onto the caller and add churn for
// no clarity gain — the pattern matches compose_with_inputs and attune.
#[allow(clippy::too_many_arguments)]
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): apply_one_pending needs to delete stale failed
// ledger rows via `DELETE FROM djogi_schema_migrations WHERE version = $1`.
// The public API has no delete operation — `select_all_ledger_rows` is read-only and
// `insert_pending` is write-only. This is the minimal raw SQL surface
// required for crash recovery.
async fn apply_one_pending(
    ctx: &mut djogi::context::DjogiContext,
    workspace: &Path,
    pending_path: &Path,
    bucket_database: String,
    app_label: String,
    config: &djogi::config::DjogiConfig,
    guard: &djogi::migrate::WorkspaceGuard,
    audit_pool: Option<&deadpool_postgres::Pool>,
    mode: &FakeMode,
) -> ApplyResult {
    // 1. Parse pending JSON to get bucket + version + checksums.
    let pending_bytes = match std::fs::read(pending_path) {
        Ok(b) => b,
        Err(e) => {
            return ApplyResult::Refused(format!("read pending JSON: {e}"));
        }
    };
    let pending: PendingPlan = match serde_json::from_slice(&pending_bytes) {
        Ok(p) => p,
        Err(e) => {
            return ApplyResult::Refused(format!("parse pending JSON: {e}"));
        }
    };

    // Resolve bucket key from pending plan fields. The `_global_` app
    // maps to empty string (synthetic global bucket).
    let resolved_app = if app_label == "_global_" {
        String::new()
    } else {
        app_label.clone()
    };
    let bucket = djogi::migrate::BucketKey {
        database: bucket_database,
        app: resolved_app,
    };

    // 2. Check ledger state machine for this version.
    match check_ledger_state(ctx, &pending.version).await {
        LedgerState::NotPresent => {} /* normal path */
        LedgerState::AlreadyApplied => {
            return ApplyResult::Skipped("already applied".to_string());
        }
        LedgerState::PendingOrPartial(existing_status) => {
            // Pending or partial state from a previous interrupted run.
            // Failed and RolledBack are non-terminal stale rows that block
            // re-apply — delete them and proceed. Pending rows require
            // explicit operator resolution.
            if existing_status == LedgerStatus::Failed
                || existing_status == LedgerStatus::RolledBack
            {
                // Both Failed and RolledBack rows are non-terminal stale rows
                // that block re-apply. delete_failed_ledger_row is a status-
                // agnostic DELETE by version; the name reflects the original
                // crash-recovery use case but the operation applies equally to
                // rolled-back rows.
                if let Err(e) = delete_failed_ledger_row(ctx, &pending.version).await {
                    return ApplyResult::Refused(format!(
                        "clean {} ledger row: {e}",
                        existing_status.as_db_str()
                    ));
                }
            } else {
                return ApplyResult::Refused(format!(
                    "version already in {} state — resolve before re-applying",
                    existing_status.as_db_str()
                ));
            }
        }
    }

    // 3. Load committed replay plan (or fall back to single-segment).
    let (plan, checksum_up, checksum_down) = match load_replay_plan_from_disk(
        workspace,
        &bucket,
        &pending.version,
        &pending.checksum_up,
        pending.checksum_down.as_deref(),
    ) {
        Ok(result) => result,
        Err(e) => {
            return ApplyResult::Refused(format!("load replay plan: {e}"));
        }
    };

    // 4. Construct RunnerCtx.
    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: pending.version.clone(),
        description: pending.slug.clone(),
        checksum_up,
        checksum_down,
        snapshot: Some(pending.model_snapshot.clone()),
        snapshot_path: Some(reconstruct_snapshot_path(workspace, &bucket)),
        // MigrateConfig does not derive Clone; construct from fields.
        config: djogi::config::MigrateConfig {
            concurrent_warn_relpages: config.migrate.concurrent_warn_relpages,
            strict_concurrent_warnings: config.migrate.strict_concurrent_warnings,
            pk_flip_long_tx_threshold_secs: config.migrate.pk_flip_long_tx_threshold_secs,
            pk_flip_join_table_option: config.migrate.pk_flip_join_table_option,
        },
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::default_for_config(config),
        audit_pool: audit_pool.cloned(),
    };

    // 5. Apply (or fake-apply) the plan through the library runner.
    let runner_result = match mode {
        FakeMode::Real => apply_plan(ctx, &plan, &runner_ctx, guard).await,
        FakeMode::Fake { reason } => fake_apply_plan(ctx, &plan, &runner_ctx, guard, reason).await,
    };
    match runner_result {
        Ok(_) => ApplyResult::Ok,
        Err(e) => ApplyResult::RunnerError(e),
    }
}

/// Ledger state for a given migration version.
#[derive(Debug)]
enum LedgerState {
    /// No row exists — first apply.
    NotPresent,
    /// Row exists and is in terminal applied state.
    AlreadyApplied,
    /// Row exists in a non-terminal state with the specific status.
    PendingOrPartial(LedgerStatus),
}

/// Check the ledger for an existing row matching `version`.
async fn check_ledger_state(ctx: &mut djogi::context::DjogiContext, version: &str) -> LedgerState {
    let Ok(rows) = djogi::migrate::select_all_ledger_rows(ctx).await else {
        // Ledger table might not exist yet — treat as NotPresent so
        // the runner can bootstrap it.
        return LedgerState::NotPresent;
    };

    let existing = rows.iter().find(|r| r.version == version);
    match existing {
        None => LedgerState::NotPresent,
        Some(row) => match row.status {
            LedgerStatus::Applied | LedgerStatus::Baseline | LedgerStatus::Faked => {
                LedgerState::AlreadyApplied
            }
            LedgerStatus::Pending | LedgerStatus::Failed | LedgerStatus::RolledBack => {
                LedgerState::PendingOrPartial(row.status)
            }
        },
    }
}

/// Map a [`RunnerError`] to an exit code.
///
/// All runner errors map to exit code 1 (apply failure). Exit code 2
/// is reserved for user-facing refusals that happen before the runner
/// is invoked.
fn runner_error_exit_code(_error: &RunnerError) -> i32 {
    1
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): delete_failed_ledger_row removes a stale Failed
// row so the migration can be retried. The public API has no delete
// operation for ledger rows — only select_all_ledger_rows and insert_pending
// are exposed. This DELETE is the minimal raw SQL required for crash recovery.
async fn delete_failed_ledger_row(
    ctx: &mut djogi::context::DjogiContext,
    version: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ctx.raw_execute(
        "DELETE FROM djogi_schema_migrations WHERE version = $1",
        &[&version],
    )
    .await?;
    Ok(())
}

/// Reconstruct the snapshot path for a bucket: `migrations/<database>/<app>/schema_snapshot.json`.
fn reconstruct_snapshot_path(workspace: &Path, bucket: &djogi::migrate::BucketKey) -> PathBuf {
    let migrations_root = djogi::migrate::migrations_root(workspace);
    migrations_root
        .join(&bucket.database)
        .join(djogi::migrate::app_dirname(&bucket.app))
        .join("schema_snapshot.json")
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

/// `djogi migrations verify` entry point.
///
/// Read-only — does not acquire the workspace lock. Reads the live
/// Postgres catalog via [`djogi::context::DjogiContext`] and compares
/// against the projected schema from the descriptor inventory.
///
/// Exit codes: 0 on success (no error-level diagnostics), 1 on runtime
/// error (config / network / SQL / projection), 2 on refusal
/// (below PG 18).
pub fn verify_cmd(workspace: Option<PathBuf>, strict: bool) -> ExitCode {
    let workspace = resolve_workspace(workspace);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations verify: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = runtime.block_on(async { run_verify(&workspace, strict).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`verify_cmd`]. Returns the desired exit code.
async fn run_verify(workspace: &Path, strict: bool) -> i32 {
    use djogi::config::DjogiConfig;

    // 1. Load config from workspace
    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations verify: config load: {e}");
            return 1;
        }
    };

    // 2. Build context with PG version check
    let mut ctx = match build_status_context(&config.database.url).await {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("djogi migrations verify: pool: {e}");
            return 1;
        }
    };

    // 3. Project schema from descriptor inventory
    let models = match project_from_inventory() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("djogi migrations verify: projection error: {e}");
            return 1;
        }
    };

    if models.is_empty() {
        println!("No registered apps found for verification.");
        return 0;
    }

    // Policy configuration for --strict flag
    let policy = djogi::config::PolicyConfig {
        strict_out_of_order: strict,
    };

    let mut exit_code = 0;
    for bucket in models.keys() {
        let snap_path = snapshot_path(workspace, bucket);
        let snapshot = match load_snapshot(&snap_path) {
            Ok(s) => s,
            Err(SnapshotError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                let _bd = if bucket.app.is_empty() {
                    "_global_"
                } else {
                    &bucket.app
                };
                println!("No snapshot found for bucket {}/{}", bucket.database, _bd);
                continue;
            }
            Err(e) => {
                let _bd = if bucket.app.is_empty() {
                    "_global_"
                } else {
                    &bucket.app
                };
                eprintln!(
                    "djogi migrations verify: load snapshot for {}/{}: {e}",
                    bucket.database, _bd
                );
                exit_code = 1;
                continue;
            }
        };

        let report = if policy.strict_out_of_order {
            djogi::migrate::verify_with_policy(&mut ctx, &snapshot, &policy).await
        } else {
            djogi::migrate::verify(&mut ctx, &snapshot).await
        };
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                let _bd = if bucket.app.is_empty() {
                    "_global_"
                } else {
                    &bucket.app
                };
                eprintln!(
                    "djogi migrations verify: run for {}/{}: {e}",
                    bucket.database, _bd
                );
                exit_code = 1;
                continue;
            }
        };

        render_verify_report(&report, bucket);
        if report.has_errors() {
            exit_code = 1;
        }
    }

    exit_code
}

/// Render a [`VerifyReport`] to stdout.
///
/// Format: one line per diagnostic with severity prefix, code, location,
/// and message. Summary line at the end. Output is deterministic
/// because `report.diagnostics` is already sorted by `(code, location)`.
fn render_verify_report(report: &VerifyReport, bucket: &BucketKey) {
    let app_display = if bucket.app.is_empty() {
        "_global_"
    } else {
        &bucket.app
    };
    println!(
        "djogi migrations verify — {}/{}",
        bucket.database, app_display
    );
    println!("──────────────────────────────────────────");

    match (
        &report.latest_applied_version,
        report.applied_count,
        report.unfinished_count,
    ) {
        (Some(version), applied, 0) => {
            println!("Ledger: {applied} applied, latest {version}");
        }
        (Some(version), applied, unfinished) => {
            println!("Ledger: {applied} applied, {unfinished} unfinished, latest {version}");
        }
        (None, 0, 0) => {
            println!("Ledger: empty (no migrations applied yet)");
        }
        _ => {}
    }
    println!();

    if report.diagnostics.is_empty() {
        println!("No drift detected. Schema is consistent.");
    } else {
        for d in &report.diagnostics {
            let severity = match d.severity {
                VerifySeverity::Info => "INFO",
                VerifySeverity::Warning => "WARN",
                VerifySeverity::Error => "ERROR",
            };
            let location = d.location.as_deref().unwrap_or("-");
            println!(
                "[{severity}] {code} ({loc}): {msg}",
                severity = severity,
                code = d.code,
                loc = location,
                msg = d.message
            );
        }
    }

    let errors = report
        .diagnostics
        .iter()
        .filter(|d| d.severity == VerifySeverity::Error)
        .count();
    let warnings = report
        .diagnostics
        .iter()
        .filter(|d| d.severity == VerifySeverity::Warning)
        .count();
    let infos = report
        .diagnostics
        .iter()
        .filter(|d| d.severity == VerifySeverity::Info)
        .count();

    if errors > 0 {
        println!();
        println!("Result: FAILED ({errors} error(s), {warnings} warning(s), {infos} info(s))");
    } else if warnings > 0 {
        println!();
        println!("Result: PASSED with warnings ({warnings} warning(s), {infos} info(s))");
    } else {
        println!();
        println!("Result: PASSED ({} info(s))", infos);
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
                    comment: None,
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
                    type_change_using: None,
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
                table_comment: None,
                storage_params: None,
                tablespace: None,
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
            // Up file pattern: starts with "V", ends with ".sdjql", does
            // NOT contain ".down.".
            if n.starts_with('V') && n.ends_with(".sdjql") && !n.contains(".down.") {
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
                path: PathBuf::from("/tmp/x.sdjql"),
                source: std::io::Error::other("permission denied"),
            },
            AttuneError::SqlWriteFailed {
                path: PathBuf::from("/tmp/x.sdjql"),
                source: std::io::Error::other("read-only fs"),
            },
            AttuneError::SqlDeleteFailed {
                path: PathBuf::from("/tmp/x.sdjql"),
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

    // ── REQ-326: --fake / --reason validation tests ─────────────────────

    /// REQ-326-5: --fake without --reason must exit with code 2.
    #[test]
    fn fake_without_reason_exits_code_2() {
        let result = apply_cmd(
            Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws")),
            true,
            None,
        );
        assert_eq!(
            result,
            ExitCode::from(2),
            "--fake without --reason must exit 2"
        );
    }

    /// REQ-326-5: --fake with blank reason must exit with code 2.
    #[test]
    fn fake_with_empty_reason_exits_code_2() {
        let result = apply_cmd(
            Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws")),
            true,
            Some(String::new()),
        );
        assert_eq!(
            result,
            ExitCode::from(2),
            "--fake with empty reason must exit 2"
        );
    }

    /// REQ-326-5: --fake with whitespace-only reason must exit with code 2.
    #[test]
    fn fake_with_whitespace_reason_exits_code_2() {
        let result = apply_cmd(
            Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws")),
            true,
            Some("   ".to_string()),
        );
        assert_eq!(
            result,
            ExitCode::from(2),
            "--fake with whitespace reason must exit 2"
        );
    }

    /// --reason without --fake is accepted (silently ignored).
    #[test]
    fn reason_without_fake_is_accepted() {
        // This should NOT exit 2; it will proceed to config load which
        // may fail on nonexistent workspace, but the --reason flag itself
        // is accepted. We verify the function does not early-exit with code 2.
        let result = apply_cmd(
            Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws")),
            false, // NOT fake
            Some("test reason".to_string()),
        );
        // Should be 1 (config error) not 2 (refusal)
        assert_ne!(
            result,
            ExitCode::from(2),
            "--reason without --fake should not refuse"
        );
    }

    #[test]
    fn render_verify_report_clean_output() {
        use djogi::migrate::{BucketKey, VerifyReport, VerifySeverity};

        let report = VerifyReport {
            diagnostics: vec![],
            latest_applied_version: Some("001_initial".to_string()),
            applied_count: 3,
            unfinished_count: 0,
        };
        let bucket = BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };

        // Just verify it doesn't panic and runs cleanly.
        // The output goes to stdout which we can't easily capture in a unit test,
        // but the function being called proves the types are correct and
        // the render logic compiles against the library types.
        render_verify_report(&report, &bucket);
    }

    #[test]
    fn render_verify_report_with_errors() {
        use djogi::migrate::{BucketKey, VerifyDiagnostic, VerifyReport, VerifySeverity};

        let report = VerifyReport {
            diagnostics: vec![
                VerifyDiagnostic {
                    code: "D601".to_string(),
                    severity: VerifySeverity::Error,
                    message: "Snapshot table missing from live DB".to_string(),
                    location: Some("users".to_string()),
                },
                VerifyDiagnostic {
                    code: "D611".to_string(),
                    severity: VerifySeverity::Warning,
                    message: "Live index not present in snapshot".to_string(),
                    location: Some("idx_posts_created".to_string()),
                },
            ],
            latest_applied_version: Some("V20260501000000__add_users".to_string()),
            applied_count: 2,
            unfinished_count: 0,
        };
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "myapp".to_string(),
        };

        assert!(report.has_errors());
        render_verify_report(&report, &bucket);
    }
}
