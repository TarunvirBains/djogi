//! `djogi migrations` subcommand glue
//! Two leaves: `compose` and `status`. Both flow through the public
//! `djogi::migrate` API. Compose acquires the workspace file lock for
//! the duration of the call; status is read-only and does not.
//! The CLI surface here is intentionally thin — all the real logic
//! lives in the library so integration tests can exercise it without
//! spawning subprocesses.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use djogi::apps::AppRegistry;
use djogi::migrate::{
    AppLifecycle, AttuneError, AttuneMode, AttuneRequest, BucketKey, ComposeError, ComposeRequest,
    DescriptorProvider, GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME, PartialApplyResolution, PendingPlan,
    RepairConfirmation, RepairError, RepairReport, RunnerCtx, RunnerError, SnapshotError,
    VerifyReport, VerifySeverity, acquire_workspace_lock, apply_plan, attune, baseline_plan,
    compose, fake_apply_plan, load_snapshot, project_from_provider, repair_checksum_drift,
    repair_partial_apply, repair_resume_partial_apply, repair_snapshot_rebuild, snapshot_path,
};

// Re-export for the apply command's ledger state machine.
use djogi::migrate::LedgerStatus;

// CLI-side enums declared at the crate root (`main.rs` is the binary's
// root module — there is no `mod main`), reached here as `crate::*`.
use crate::{PartialApplyResolutionCli, RepairSubcommand};

// ── Replay plan deserialization ──────────────────────────────────────────

/// Local mirror of `StoredReplayPlan` (pub(crate) in the library).
/// The committed replay plan JSON written by `compose` at
/// `migrations/<database>/<app>/<version>.plan.json`. This struct
/// allows the CLI to parse it and construct a proper [`MigrationPlan`]
/// with correct segment structure and checksums.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct CliReplayPlan {
    format_version: String,
    checksum_up: String,
    checksum_down: Option<String>,
    classification: CliClassification,
    segments: Vec<CliReplaySegment>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct CliReplaySegment {
    kind: CliSegmentKind,
    statements: Vec<CliReplayStatement>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CliSegmentKind {
    Transactional,
    NonTransactional,
    MetadataOnly,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct CliReplayStatement {
    label: String,
    up: String,
}

/// Expected format version for the committed replay plan JSON.
const CLI_REPLAY_PLAN_FORMAT_VERSION: &str = "1";

/// Load the committed replay plan from disk and convert to a
/// [`djogi::migrate::MigrationPlan`]. Returns `(plan, checksum_up, checksum_down)`.
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

/// Classify a Phase 0 artifact for the CLI cleanup path (#386).
/// Loads the committed replay plan JSON or falls back to the SQL file,
/// classifies the up SQL using [`djogi::migrate::classify_phase_zero_artifact`],
/// and returns `Some(reason)` unless the artifact is identity-free
/// replay-current.
/// Returns `None` when the artifact is safe for migration replay.
fn classify_phase_zero_for_cleanup(
    workspace: &Path,
    bucket: &djogi::migrate::BucketKey,
    version: &str,
    pending_checksum_up: &str,
    pending_checksum_down: Option<&str>,
) -> Option<String> {
    // Try to load the committed replay plan JSON first.
    let bucket_dir = djogi::migrate::bucket_dir(workspace, bucket);
    let replay_plan_path = bucket_dir.join(format!("{version}.plan.json"));

    if let Ok(bytes) = std::fs::read(&replay_plan_path) {
        let stored: CliReplayPlan = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(e) => {
                return Some(format!("parse replay plan: {e}"));
            }
        };

        if stored.format_version != CLI_REPLAY_PLAN_FORMAT_VERSION {
            return Some(format!(
                "replay plan format version mismatch: expected {}, found {}",
                CLI_REPLAY_PLAN_FORMAT_VERSION, stored.format_version
            ));
        }

        // Verify checksums match the pending plan.
        if stored.checksum_up != pending_checksum_up
            || stored.checksum_down.as_deref() != pending_checksum_down
        {
            return Some("checksum mismatch between pending JSON and replay plan".to_string());
        }

        // Reconstruct the up SQL from the replay plan segments for classification.
        let up_sql: String = stored
            .segments
            .iter()
            .flat_map(|seg| seg.statements.iter())
            .map(|stmt| stmt.up.as_str())
            .collect::<Vec<&str>>()
            .join("\n");

        return classify_phase_zero_bytes(up_sql.as_bytes());
    }

    // Fallback: read the up SQL file directly.
    let up_filename = djogi::migrate::up_filename(version);
    let up_path = bucket_dir.join(&up_filename);
    match std::fs::read_to_string(&up_path) {
        Ok(up_sql) => classify_phase_zero_bytes(up_sql.as_bytes()),
        Err(e) => Some(format!("read up SQL file {}: {e}", up_path.display())),
    }
}

/// Classify raw bytes as Phase 0 artifact and return refusal reason unless it
/// is identity-free replay-current.
fn classify_phase_zero_bytes(bytes: &[u8]) -> Option<String> {
    match djogi::migrate::classify_phase_zero_artifact(bytes) {
        djogi::migrate::PhaseZeroArtifactState::IdentityFreeCurrent => None,
        djogi::migrate::PhaseZeroArtifactState::SeedCapableRuntimeCurrent => {
            Some("seed-capable runtime-only artifact detected".to_string())
        }
        djogi::migrate::PhaseZeroArtifactState::SeedDmlNotRuntimeCurrent => {
            Some("seed-dml non-runtime-current artifact detected".to_string())
        }
        djogi::migrate::PhaseZeroArtifactState::GeneratedStale => {
            Some("generated-stale artifact detected".to_string())
        }
        djogi::migrate::PhaseZeroArtifactState::Ambiguous => {
            Some("ambiguous or hand-edited artifact detected".to_string())
        }
        djogi::migrate::PhaseZeroArtifactState::Incomplete => {
            Some("incomplete artifact (truncated generation)".to_string())
        }
        djogi::migrate::PhaseZeroArtifactState::Missing => Some("missing artifact".to_string()),
    }
}

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
/// Compose's `snapshots` map must include the OLD bucket of any
/// renamed app — and that bucket is guaranteed to be absent from the
/// current `models` inventory because the `#[app(renamed_from =
/// "old")]` annotation lives on the NEW app. Walking disk directly
/// recovers those orphaned snapshots so the differ sees both sides of
/// a rename.
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
    provider: &dyn DescriptorProvider,
    name: &str,
    allow_destructive: bool,
    force_overwrite: bool,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let models = match project_from_provider(provider) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("djogi migrations compose: projection error: {e}");
            return ExitCode::from(1);
        }
    };
    let apps: Vec<AppLifecycle> = provider
        .apps()
        .iter()
        .map(|d| AppLifecycle {
            label: d.label.to_string(),
            database: d.database.to_string(),
            renamed_from: d.renamed_from.map(str::to_string),
            tombstone: d.tombstone,
        })
        .collect();
    // The resolved workspace flows into config loading. Compose consumes
    // the [`MigrateConfig::pk_flip_join_table_option`] knob so we no
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
/// Acquires the workspace lock, walks the on-disk migration tree to
/// recover orphaned snapshots (renamed-from buckets), and
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

    // Read snapshots from disk. The bucket set we load is the UNION of
    // (a) every bucket the current projection knows about and (b) every
    // on-disk bucket that has a snapshot file.
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
        // Production: always run auto-emit. The flag is a
        // test-only escape hatch for unit tests that exercise
        // compose's lower-level write/rollback machinery in
        // isolation; the CLI / production path always goes through
        // the full bootstrap flow.
        skip_phase_zero_auto_emit: false,
    };
    match compose(req) {
        Ok(report) => {
            // Surface auto-emitted bootstraps before
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
                    "auto-emitted bootstrap migration: {database}/_global_ ({ext_summary})",
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
            // Per the inline-decisions: nothing-to-compose is
            // not an error. The status command is the one that
            // signals out-of-sync state via exit code.
            ExitCode::from(0)
        }
        Err(ComposeError::LinkageDropWithoutModels { ref text, .. }) => {
            eprintln!("djogi migrations compose: {text}");
            // Exit 2 — refusal: models must be compiled in before dropping app linkage.
            ExitCode::from(2)
        }
        Err(e @ ComposeError::CrossBucketForeignKeyCycle { .. }) => {
            eprintln!("djogi migrations compose: {e}");
            // Exit 2 — operator-actionable refusal: the operator resolves the
            // cycle (merge the apps or drop one FK direction). A blind retry
            // would refuse identically, so this is exit 2, not the exit-1
            // unexpected-error catch-all below.
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("djogi migrations compose: {e}");
            ExitCode::from(1)
        }
    }
}

/// `djogi migrations status` entry point.
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
/// The resolved `workspace` path feeds
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

    let mut ctx = match connect_and_check(&config.database.url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations status", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations status: pool: {msg}");
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

/// Outcome of [`connect_and_check`] — connecting a pool and running the
/// Postgres-version preflight, with the support-boundary refusal kept
/// distinct from ordinary runtime failures.
/// The three arms drive different exit codes at the call site:
/// - [`ContextOutcome::Ready`] — pool connected and PG ≥ 18; proceed.
/// - [`ContextOutcome::UnsupportedVersion`] — PG < 18. The caller renders
///   the support-boundary message via
///   [`crate::print_support_boundary_error`] and exits `2` (refusal: the
///   operator must upgrade Postgres; retrying changes nothing).
/// - [`ContextOutcome::RuntimeError`] — pool connect failed, the preflight
///   query errored, or any other non-version `DjogiError`. The caller
///   prints the message and exits `1` (transient: CI may retry).
// The `Ready` variant holds a `DjogiContext` (large — it wraps a
// `DjogiPool`), while the other two variants are small (`DjogiError` /
// `String`). Boxing `Ready` would add a heap allocation on the success
// path; this value is constructed and immediately matched at each call
// site (never stored in a collection), so the wider stack value is a
// transient one-off, not a per-element penalty. Same trade-off and
// rationale as `ContextInner` in `djogi::context` (see its
// `large_enum_variant` allow).
#[allow(clippy::large_enum_variant)]
enum ContextOutcome {
    /// Pool connected and the PG-version preflight passed.
    Ready(djogi::context::DjogiContext),
    /// The PG-version preflight refused — server is below the minimum
    /// supported major version.
    UnsupportedVersion(djogi::error::DjogiError),
    /// A runtime failure (connect / preflight / other) — already rendered
    /// to a string so the call site need not re-match.
    RuntimeError(String),
}

/// Connect a pool from `url` and run the Postgres-version preflight,
/// returning a typed [`ContextOutcome`].
/// Splits the support-boundary refusal (PG < 18, exit `2`) from runtime
/// failures (connect / query errors, exit `1`) so each call site can map
/// the outcome onto the documented exit-code matrix. Connects via the
/// public `DjogiPool::connect` entry point, then hands the pool to the
/// public `DjogiContext::from_pool` API once the version check passes.
async fn connect_and_check(url: &str) -> ContextOutcome {
    let pool = match djogi::pg::pool::DjogiPool::connect(url).await {
        Ok(p) => p,
        Err(e) => return ContextOutcome::RuntimeError(e.to_string()),
    };
    match djogi::pg::preflight::check_postgres_version(&pool).await {
        Ok(_) => ContextOutcome::Ready(djogi::context::DjogiContext::from_pool(pool)),
        // `DjogiError` is `#[non_exhaustive]`, so the `@`-bound
        // `UnsupportedPostgresVersion` arm needs the trailing `_` catch-all.
        Err(e @ djogi::error::DjogiError::UnsupportedPostgresVersion { .. }) => {
            ContextOutcome::UnsupportedVersion(e)
        }
        Err(other) => ContextOutcome::RuntimeError(other.to_string()),
    }
}

/// Resolve the connection URL for a single migration-bucket database.
/// Verify routes each bucket to the pool for its `database` component.
/// The mapping mirrors Djogi's three-database architecture:
/// - `"main"` ([`djogi::apps::AppDescriptor::GLOBAL_DATABASE`]) always uses
///   the app URL verbatim. We do NOT derive it by splicing `"main"` into
///   the path, because the operator's app URL may carry a path component
///   that is not literally named `main` (e.g. `…/myapp_prod`); deriving
///   would target a database that does not exist.
/// - `"crud_log"` / `"event_log"` prefer the explicit
///   [`djogi::config::DatabaseConfig::crud_log_url`] /
///   [`event_log_url`](djogi::config::DatabaseConfig::event_log_url) when
///   set to a non-empty value, matching how the audit / event pools are
///   resolved elsewhere.
/// - Any other database name (and the log databases when their explicit
///   URL is absent) is derived by splicing the name into the app URL's
///   path component via [`djogi::migrate::derive_per_database_url`].
///   Returns `None` when derivation fails (the app URL has no recognisable
///   path component); the caller surfaces that as a runtime error for the
///   affected bucket.
fn resolve_bucket_url(db_config: &djogi::config::DatabaseConfig, database: &str) -> Option<String> {
    // "main" always uses the app URL verbatim — do NOT derive, as the app
    // URL may not have a path component named "main".
    if database == djogi::apps::AppDescriptor::GLOBAL_DATABASE {
        return Some(db_config.url.clone());
    }
    if database == "crud_log"
        && let Some(u) = db_config.crud_log_url.as_deref()
        && !u.is_empty()
    {
        return Some(u.to_string());
    }
    if database == "event_log"
        && let Some(u) = db_config.event_log_url.as_deref()
        && !u.is_empty()
    {
        return Some(u.to_string());
    }
    djogi::migrate::derive_per_database_url(&db_config.url, database)
}

/// `djogi migrations apply` entry point.
/// Discovers pending JSON files under `target/djogi_pending/`, loads the
/// committed replay plan for each, and drives [`djogi::migrate::apply_plan`]
/// through the library runner after CLI-side ledger-state classification.
/// `Pending` rows require operator resolution. Caller-gated `Failed`/`RolledBack`
/// rows are reapply-blocking cleanup candidates before runner invocation. Phase
/// 0 cleanup is identity-free replay-current-only: seed-capable runtime,
/// seed-DML non-runtime-current, missing, incomplete, generated-stale, or
/// ambiguous artifacts refuse before delete.
pub fn apply_cmd(
    workspace: Option<PathBuf>,
    fake: bool,
    reason: Option<String>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> ExitCode {
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

    let exit =
        runtime.block_on(async { run_apply(&workspace, &mode, node_id, single_node_dev).await });
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
async fn run_apply(
    workspace: &Path,
    mode: &FakeMode,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> i32 {
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

    // 2. Discover pending JSONs before resolving identity or connecting to DB.
    // No-pending apply (zero pending files) is an identity-free inverse —
    // skip the resolver and pool connection entirely when no pending plans exist.
    let pending_files = match discover_pending_plans(workspace) {
        Ok(pending_files) => pending_files,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: pending discovery: {e}");
            return 2;
        }
    };
    if pending_files.is_empty() {
        println!("No pending migrations to {action_verb}.");
        return 0;
    }

    // 3. Resolve node identity for identity-bearing operations (only when work exists).
    // Both real apply and fake-apply are identity-bearing (run-id generation + ledger).
    let runner_identity = match crate::identity::resolve_identity(
        node_id,
        single_node_dev,
        &config.profile,
        action_verb,
    ) {
        Ok(resolved) => Some(resolved.into_runner_identity()),
        Err(e) => {
            let _ = crate::identity::print_identity_error(action_verb, &e);
            return 2;
        }
    };

    // 4. Resolve one URL per pending database target, then connect and
    // preflight a dedicated context for each database before taking the
    // workspace lock. The runner routes queries through the supplied
    // context pool, so apply must bind one context per bucket.database.
    let target_urls = match resolve_apply_target_urls(&pending_files, &config.database) {
        Ok(urls) => urls,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: target routing: {e}");
            return 2;
        }
    };
    let mut contexts = std::collections::BTreeMap::<String, djogi::context::DjogiContext>::new();
    for (database, url) in &target_urls {
        match connect_and_check(url).await {
            ContextOutcome::Ready(ctx) => {
                contexts.insert(database.clone(), ctx);
            }
            ContextOutcome::UnsupportedVersion(e) => {
                crate::print_support_boundary_error("migrations apply", &e);
                return 2;
            }
            ContextOutcome::RuntimeError(msg) => {
                eprintln!("djogi migrations {action_verb}: pool for '{database}': {msg}");
                return 1;
            }
        }
    }

    // 5. Acquire workspace lock.
    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: workspace lock: {e}");
            return 1;
        }
    };

    // 6. Reconcile the pending set under the lock before any cleanup/apply work.
    let pending_files = match reconcile_pending_plans_after_lock(workspace, &pending_files) {
        Ok(pending_files) => pending_files,
        Err(e) => {
            eprintln!("djogi migrations {action_verb}: pending discovery: {e}");
            return 2;
        }
    };

    // 7. Build audit pool (optional — silently skipped if unavailable).
    let audit_pool = match djogi::migrate::resolve_audit_url(&config) {
        Ok(url) => djogi::migrate::build_audit_pool(&url).await.ok(),
        Err(_) => None,
    };

    // 8. Apply each pending migration through the context for its
    // bucket database. The pending discovery sweep already deduped and
    // preflighted the target database set above.
    for pending_file in &pending_files {
        let bucket_database = &pending_file.bucket.database;
        let app_label = &pending_file.bucket.app;
        let Some(ctx) = contexts.get_mut(bucket_database) else {
            eprintln!(
                "djogi migrations {action_verb}: internal error: missing context for database '{bucket_database}'"
            );
            return 1;
        };
        println!("  {progress_verb} {bucket_database}/{app_label}...");
        let result = apply_one_pending(
            ctx,
            workspace,
            pending_file,
            &config,
            &guard,
            audit_pool.as_ref(),
            mode,
            runner_identity,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredPendingPlan {
    path: PathBuf,
    bucket: BucketKey,
    plan: PendingPlan,
    is_phase_zero: bool,
}

fn is_acceptable_pending_path_component(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    if bytes[0] == b'.' {
        return false;
    }
    let first = bytes[0];
    if first != b'_' && !first.is_ascii_alphabetic() {
        return false;
    }
    for &b in &bytes[1..] {
        if b != b'_' && !b.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

fn canonical_pending_filename(app_label: &str) -> String {
    format!("{}.json", djogi::migrate::app_dirname(app_label))
}

fn validate_hidden_phase_zero_pending(
    path: PathBuf,
    database: &str,
) -> Result<DiscoveredPendingPlan, String> {
    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| format!("non-utf8 Phase 0 pending path {}", path.display()))?;
    let expected_filename = format!("{}.json", djogi::migrate::PHASE_ZERO_VERSION);
    if filename != expected_filename {
        return Err(format!(
            "hidden Phase 0 pending path {} must use canonical filename {}",
            path.display(),
            expected_filename
        ));
    }
    let plan = djogi::migrate::load_pending(&path)
        .map_err(|e| format!("parse pending JSON {}: {e}", path.display()))?;
    if plan.bucket_database != database {
        return Err(format!(
            "pending JSON {} has bucket database {}, expected {} from path",
            path.display(),
            plan.bucket_database,
            database
        ));
    }
    if !plan.bucket_app.is_empty() {
        return Err(format!(
            "pending JSON {} must target the global bucket in hidden Phase 0 namespace",
            path.display()
        ));
    }
    if plan.version != djogi::migrate::PHASE_ZERO_VERSION {
        return Err(format!(
            "pending JSON {} must use Phase 0 version {}, found {}",
            path.display(),
            djogi::migrate::PHASE_ZERO_VERSION,
            plan.version
        ));
    }
    Ok(DiscoveredPendingPlan {
        path,
        bucket: BucketKey {
            database: database.to_string(),
            app: String::new(),
        },
        plan,
        is_phase_zero: true,
    })
}

fn validate_normal_pending(
    path: PathBuf,
    database: &str,
    filename: &str,
) -> Result<DiscoveredPendingPlan, String> {
    let Some(stem) = filename.strip_suffix(".json") else {
        return Err(format!(
            "pending path {} must end with .json",
            path.display()
        ));
    };
    let app = if stem == "_global_" {
        String::new()
    } else {
        if !is_acceptable_pending_path_component(stem.as_bytes()) {
            return Err(format!(
                "pending path {} uses non-canonical app filename {}",
                path.display(),
                filename
            ));
        }
        stem.to_string()
    };
    let expected_filename = canonical_pending_filename(&app);
    if filename != expected_filename {
        return Err(format!(
            "pending path {} must use canonical filename {}",
            path.display(),
            expected_filename
        ));
    }
    let plan = djogi::migrate::load_pending(&path)
        .map_err(|e| format!("parse pending JSON {}: {e}", path.display()))?;
    if plan.bucket_database != database {
        return Err(format!(
            "pending JSON {} has bucket database {}, expected {} from path",
            path.display(),
            plan.bucket_database,
            database
        ));
    }
    if plan.bucket_app != app {
        let expected_app = if app.is_empty() {
            "_global_"
        } else {
            app.as_str()
        };
        let found_app = if plan.bucket_app.is_empty() {
            "_global_"
        } else {
            plan.bucket_app.as_str()
        };
        return Err(format!(
            "pending JSON {} has bucket app {}, expected {} from path",
            path.display(),
            found_app,
            expected_app
        ));
    }
    if plan.version == djogi::migrate::PHASE_ZERO_VERSION {
        return Err(format!(
            "pending JSON {} must use the hidden .phase_zero namespace for Phase 0",
            path.display()
        ));
    }
    Ok(DiscoveredPendingPlan {
        path,
        bucket: BucketKey {
            database: database.to_string(),
            app,
        },
        is_phase_zero: false,
        plan,
    })
}

/// Scan `target/djogi_pending/` for pending JSON files.
/// Returns parsed pending plans sorted by version so Phase 0 runs
/// before later normal-global work. Malformed or duplicate pending
/// identities refuse rather than being guessed from filenames.
fn discover_pending_plans(workspace: &Path) -> Result<Vec<DiscoveredPendingPlan>, String> {
    let pending_root = djogi::migrate::pending_root(workspace);
    let mut out = Vec::new();
    let mut seen_identities = std::collections::BTreeSet::new();

    let Ok(db_entries) = std::fs::read_dir(&pending_root) else {
        return Ok(out);
    };

    for db_entry in db_entries.flatten() {
        let db_name = match db_entry.file_name().to_str().map(str::to_string) {
            Some(n) => n,
            None => continue,
        };
        if !is_acceptable_pending_path_component(db_name.as_bytes()) {
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
            let file_type = match app_entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                if app_entry.file_name().to_str() == Some(".phase_zero") {
                    let Ok(phase_zero_entries) = std::fs::read_dir(&path) else {
                        continue;
                    };
                    for phase_zero_entry in phase_zero_entries.flatten() {
                        let phase_zero_path = phase_zero_entry.path();
                        if !phase_zero_path.is_file() {
                            continue;
                        }
                        let discovered =
                            validate_hidden_phase_zero_pending(phase_zero_path, &db_name)?;
                        let identity = (
                            discovered.bucket.database.clone(),
                            discovered.bucket.app.clone(),
                            discovered.plan.version.clone(),
                        );
                        if !seen_identities.insert(identity.clone()) {
                            return Err(format!(
                                "duplicate pending identity discovered for {}/{}/{}",
                                identity.0,
                                if identity.1.is_empty() {
                                    "_global_"
                                } else {
                                    identity.1.as_str()
                                },
                                identity.2
                            ));
                        }
                        out.push(discovered);
                    }
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let filename = match path.file_name().and_then(|f| f.to_str()) {
                Some(f) => f.to_string(),
                None => continue,
            };
            if !filename.ends_with(".json") {
                continue;
            }
            let discovered = validate_normal_pending(path, &db_name, &filename)?;
            let identity = (
                discovered.bucket.database.clone(),
                discovered.bucket.app.clone(),
                discovered.plan.version.clone(),
            );
            if !seen_identities.insert(identity.clone()) {
                return Err(format!(
                    "duplicate pending identity discovered for {}/{}/{}",
                    identity.0,
                    if identity.1.is_empty() {
                        "_global_"
                    } else {
                        identity.1.as_str()
                    },
                    identity.2
                ));
            }
            out.push(discovered);
        }
    }

    // Stage 1 — global stable order: version, then phase-zero precedence,
    // then path (also the within-group tiebreak seed).
    out.sort_by(|a, b| {
        a.plan
            .version
            .cmp(&b.plan.version)
            .then_with(|| b.is_phase_zero.cmp(&a.is_phase_zero))
            .then_with(|| a.path.cmp(&b.path))
    });

    // Stage 2: within each (database, version, is_phase_zero) group,
    // reorder by the recorded depends_on (Kahn; stage-1 alphabetical order
    // is the deterministic tiebreak). Dependencies naming buckets outside
    // the group are ignored — their migrations applied in an earlier run.
    // A cycle is a compose bug or a hand-edited pending file; refuse loudly.
    let out = order_pending_groups_by_dependencies(out)?;

    Ok(out)
}

/// Within each same-(database, version, is_phase_zero) group, reorder by
/// the recorded depends_on list using Kahn's algorithm. The stage-1 sort
/// provides a deterministic alphabetical tiebreak for nodes with equal
/// in-degree. Dependencies on buckets not present in the current group
/// are ignored (their migrations already applied). Returns an error on
/// cycle — the compose side should have caught this, but apply guards
/// against hand-edited or corrupted pending files.
///
/// Algorithmic twin of `order_buckets` in compose.rs; kept local because
/// the CLI cannot call private compose helpers across crates.
fn order_pending_groups_by_dependencies(
    out: Vec<DiscoveredPendingPlan>,
) -> Result<Vec<DiscoveredPendingPlan>, String> {
    // Group by (database, version, is_phase_zero). Since stage 1 already
    // sorted by these keys, consecutive entries share the same group.
    let mut result = Vec::with_capacity(out.len());
    let mut i = 0;
    while i < out.len() {
        let mut j = i + 1;
        while j < out.len()
            && out[j].bucket.database == out[i].bucket.database
            && out[j].plan.version == out[i].plan.version
            && out[j].is_phase_zero == out[i].is_phase_zero
        {
            j += 1;
        }

        // Validate depends_on labels for all entries in this group before
        // any topo-sort (including the singleton fast-path that bypasses it).
        // Discovery validates pending *filenames*, but depends_on labels live
        // inside the pending JSON and are otherwise unchecked — a hand-edited
        // or corrupted label (path traversal, whitespace) would slip through
        // the singleton fast-path silently.
        for entry in &out[i..j] {
            for dep_app in &entry.plan.depends_on {
                if !is_acceptable_pending_path_component(dep_app.as_bytes()) {
                    return Err(format!(
                        "pending plan for {}/{} has invalid depends_on label {:?}",
                        entry.bucket.database, entry.bucket.app, dep_app,
                    ));
                }
            }
        }

        // Process the group [i..j)
        if j - i <= 1 {
            // Single-element or empty group: no reordering needed.
            result.append(&mut out[i..j].to_vec());
            i = j;
            continue;
        }

        let database = &out[i].bucket.database;
        let version = &out[i].plan.version;

        // Build the dependency graph within this group.
        let group_len = j - i;
        let mut in_degree = vec![0usize; group_len];
        let mut reverse: Vec<Vec<usize>> = vec![Vec::new(); group_len];

        // Build app→index lookup for this group (O(n)).
        let app_to_idx: std::collections::HashMap<&str, usize> = out[i..j]
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.bucket.app.as_str(), idx))
            .collect();

        for (k_idx, entry) in out[i..j].iter().enumerate() {
            for dep_app in &entry.plan.depends_on {
                let Some(&dep_idx) = app_to_idx.get(dep_app.as_str()) else {
                    continue; // outside group — ignore (REQ-398-6)
                };
                if dep_idx != k_idx {
                    in_degree[k_idx] += 1;
                    reverse[dep_idx].push(k_idx);
                }
            }
        }

        // Kahn's algorithm with BTreeSet for deterministic (min-first) tiebreak.
        let mut ready: std::collections::BTreeSet<usize> =
            (0..group_len).filter(|&idx| in_degree[idx] == 0).collect();

        let mut ordered = Vec::with_capacity(group_len);
        while let Some(idx) = ready.iter().next().cloned() {
            ready.remove(&idx);
            ordered.push(idx);
            for &dependent in &reverse[idx] {
                in_degree[dependent] -= 1;
                if in_degree[dependent] == 0 {
                    ready.insert(dependent);
                }
            }
        }

        if ordered.len() != group_len {
            let mut chain: Vec<String> = (0..group_len)
                .filter(|&idx| in_degree[idx] > 0)
                .map(|idx| out[i + idx].bucket.app.clone())
                .collect();
            chain.sort();
            return Err(format!(
                "pending migrations for database `{database}` version `{version}` \
                 declare a dependency cycle between apps: {chain:?}; \
                 recompose or inspect hand-edited pending files"
            ));
        }

        for idx in ordered {
            result.push(out[i + idx].clone());
        }
        i = j;
    }

    Ok(result)
}

fn load_verified_pending_for_apply(
    pending_file: &DiscoveredPendingPlan,
) -> Result<PendingPlan, String> {
    let pending_bytes =
        std::fs::read(&pending_file.path).map_err(|e| format!("read pending JSON: {e}"))?;
    let pending: PendingPlan =
        serde_json::from_slice(&pending_bytes).map_err(|e| format!("parse pending JSON: {e}"))?;
    if pending != pending_file.plan {
        return Err(format!(
            "pending JSON changed after discovery at {}; rerun the command",
            pending_file.path.display()
        ));
    }
    Ok(pending)
}

fn resolve_apply_target_urls(
    pending_files: &[DiscoveredPendingPlan],
    db_config: &djogi::config::DatabaseConfig,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut urls = std::collections::BTreeMap::new();
    for pending_file in pending_files {
        let database = &pending_file.bucket.database;
        if urls.contains_key(database) {
            continue;
        }
        let Some(url) = resolve_bucket_url(db_config, database) else {
            return Err(format!("cannot derive a database URL for `{database}`"));
        };
        urls.insert(database.clone(), url);
    }
    Ok(urls)
}

fn reconcile_pending_plans_after_lock(
    workspace: &Path,
    pre_lock_pending_files: &[DiscoveredPendingPlan],
) -> Result<Vec<DiscoveredPendingPlan>, String> {
    let locked_pending_files = discover_pending_plans(workspace)?;
    if locked_pending_files != pre_lock_pending_files {
        return Err(
            "pending migration set changed while waiting for the workspace lock; rerun the command"
                .to_string(),
        );
    }
    Ok(locked_pending_files)
}

/// Apply a single pending migration.
/// Re-loads the pending JSON after discovery and refuses if the bytes no
/// longer match the path-verified artifact, then checks the ledger-state
/// classification, loads the committed replay plan (or falls back to a
/// single-segment plan from the SQL file), and drives
/// [`djogi::migrate::apply_plan`]. `Pending` rows require operator resolution;
/// caller-gated `Failed`/`RolledBack` rows are reapply-blocking cleanup
/// candidates before runner invocation. Phase 0 cleanup refuses anything other
/// than identity-free replay-current before delete.
/// Uses the bypass attribute because deleting reapply-blocking
/// Failed/RolledBack ledger rows requires raw SQL that is not exposed through
/// the public typed API.
// apply_one_pending carries 9 arguments because it sits at the bridge
// between the CLI dispatch (workspace, path, bucket info) and the
// library runner (config, guard, audit pool, mode). Folding these into a
// struct would push the same fields onto the caller and add churn for
// no clarity gain — the pattern matches compose_with_inputs and attune.
#[allow(clippy::too_many_arguments)]
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): apply_one_pending owns the shared cleanup path for
// caller-gated Failed/RolledBack rows via
// `DELETE FROM djogi_schema_migrations WHERE version = $1 AND app_label = $2`.
// The public API has no delete operation — `select_all_ledger_rows` is read-only
// and `insert_pending` is write-only. This is the minimal raw SQL surface for
// reapply-blocking ledger-row cleanup.
async fn apply_one_pending(
    ctx: &mut djogi::context::DjogiContext,
    workspace: &Path,
    pending_file: &DiscoveredPendingPlan,
    config: &djogi::config::DjogiConfig,
    guard: &djogi::migrate::WorkspaceGuard,
    audit_pool: Option<&deadpool_postgres::Pool>,
    mode: &FakeMode,
    runner_identity: Option<djogi::migrate::RunnerIdentity>,
) -> ApplyResult {
    // 1. Parse pending JSON to get bucket + version + checksums.
    let pending = match load_verified_pending_for_apply(pending_file) {
        Ok(pending) => pending,
        Err(e) => return ApplyResult::Refused(e),
    };

    let bucket = pending_file.bucket.clone();

    // 2. Check ledger state machine for this (version, app_label) stream.
    match check_ledger_state(ctx, &pending.version, &bucket.app).await {
        LedgerState::NotPresent => {} /* normal path */
        LedgerState::AlreadyApplied => {
            return ApplyResult::Skipped("already applied".to_string());
        }
        LedgerState::PendingOrPartial(existing_status) => {
            // Pending rows require explicit operator resolution.
            // Caller-gated Failed and RolledBack rows are reapply-blocking
            // cleanup candidates before runner invocation.
            if existing_status == LedgerStatus::Failed
                || existing_status == LedgerStatus::RolledBack
            {
                // #386: Phase 0 cleanup must classify before deleting.
                // Load the committed replay plan or fallback SQL first,
                // and refuse any non-identity-free Phase 0 artifact before
                // removing the failed/rolled_back row. This applies to both
                // real apply and fake apply paths.
                if pending.version == djogi::migrate::PHASE_ZERO_VERSION {
                    let cleanup_refusal = classify_phase_zero_for_cleanup(
                        workspace,
                        &bucket,
                        &pending.version,
                        &pending.checksum_up,
                        pending.checksum_down.as_deref(),
                    );
                    if let Some(reason) = cleanup_refusal {
                        return ApplyResult::Refused(format!(
                            "Phase 0 cleanup refused: {reason}; \
                             refusing before deleting {} row to prevent stale replay",
                            existing_status.as_db_str()
                        ));
                    }
                }

                // Failed and RolledBack rows both block re-apply, but callers
                // gate which statuses may be cleaned before reaching this
                // status-agnostic DELETE helper.
                if let Err(e) =
                    delete_reapply_blocking_ledger_row(ctx, &pending.version, &bucket.app).await
                {
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
        runner_identity,
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

/// Check the ledger for an existing row matching `(version, app_label)`.
async fn check_ledger_state(
    ctx: &mut djogi::context::DjogiContext,
    version: &str,
    app_label: &str,
) -> LedgerState {
    let Ok(rows) = djogi::migrate::select_all_ledger_rows(ctx).await else {
        // Ledger table might not exist yet — treat as NotPresent so
        // the runner can bootstrap it.
        return LedgerState::NotPresent;
    };

    let existing = rows
        .iter()
        .find(|r| r.version == version && r.app_label == app_label);
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
/// All runner errors map to exit code 1 (apply failure). Exit code 2
/// is reserved for user-facing refusals that happen before the runner
/// is invoked.
fn runner_error_exit_code(_error: &RunnerError) -> i32 {
    1
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): delete_reapply_blocking_ledger_row removes a caller-
// gated Failed or RolledBack row so the migration can be retried. The public
// API has no delete operation for ledger rows — only select_all_ledger_rows
// and insert_pending are exposed. This DELETE is the minimal raw SQL for
// reapply-blocking ledger-row cleanup.
async fn delete_reapply_blocking_ledger_row(
    ctx: &mut djogi::context::DjogiContext,
    version: &str,
    app_label: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ctx.raw_execute(
        "DELETE FROM djogi_schema_migrations \
         WHERE version = $1 AND app_label = $2",
        &[&version, &app_label],
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
/// Mode selection (per CLI flags):
/// | `--record-ledger` | `--squash` | resolved mode |
/// |-----------|-----------|---------------|
/// | false | false | [`AttuneMode::DiffOnly`] (read-only diff) |
/// | true | false | [`AttuneMode::Record`] |
/// | false | true | [`AttuneMode::Squash { from, publish, app }`] |
/// | true | true | rejected by clap (`conflicts_with`) |
/// Argument semantics:
/// - `target` is an optional positional Git target (commit / tag /
///   branch). When supplied, attune resolves it (local first, fetch
///   on miss) before any DB / disk mutation.
/// - `apply` gates DB / disk mutation. Without it, every mode is a
///   dry-run.
/// - `record` controls the parent repo's recorded submodule pointer
///   (separate from `record_ledger`, which controls the
///   `djogi_schema_migrations` ledger inserts).
///   `--squash` requires `--from <ver>`; an absent `from` while
///   `--squash` is set surfaces as a CLI error before any work happens.
// The CLI dispatch carries 11 inputs because the attune surface is
// the broadest in the migrations CLI — target
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

    let mut ctx = match connect_and_check(&config.database.url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations attune", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations attune: pool: {msg}");
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
        // Thread `[database].dev_mode` to the squash gate. Read-only modes
        // (`DiffOnly`, `Record`) ignore it; `Squash` mode refuses unless
        // this is `true`.
        dev_mode: config.database.dev_mode,
        // The operator-supplied target + the `--apply` / `--record` gates
        // flow through to the library
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
            // LedgerTableMissing notice when DiffOnly runs on a
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
/// - Refusal variants → exit code `2` ("operator must intervene;
///   nothing happened"). Today every refusal flows through
///   [`AttuneError::Refused`]; the localhost gate, the dev-profile
///   gate, the missing-version refusal, and the ambiguous-version
///   refusal are all reachable through that variant.
/// - Runtime variants → exit code `1` ("we tried; something broke"
///   filesystem scan, ledger query, SQL read/write/delete, git
///   publish). CI may safely retry these.
///   Pulled out as a free function so unit tests can pin every variant
///   without spinning a Tokio runtime. Operators rely on the 1-vs-2
///   distinction to tell "refused before any side effect" from "ran and
///   failed mid-flight".
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
/// Read-only — does not acquire the workspace lock. Reads the live
/// Postgres catalog via [`djogi::context::DjogiContext`] and compares
/// against the projected schema from the descriptor inventory.
/// Exit codes: 0 on success (no error-level diagnostics), 1 on runtime
/// error (config / network / SQL / projection), 2 on refusal
/// (below PG 18).
pub fn verify_cmd(
    provider: &dyn DescriptorProvider,
    workspace: Option<PathBuf>,
    strict: bool,
) -> ExitCode {
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

    let exit = runtime.block_on(async { run_verify(provider, &workspace, strict).await });
    ExitCode::from(exit as u8)
}

/// Async body of [`verify_cmd`]. Returns the desired exit code.
/// Verify is multi-database aware: each `(database, app)` bucket is routed
/// to the pool for its `database` component via [`resolve_bucket_url`], and
/// the per-database context is connected lazily and cached so a database
/// with several app buckets connects once. The bucket set is the UNION of
/// the inventory projection and the on-disk snapshot tree, so an orphaned
/// snapshot (a removed app's snapshot still on disk) is verified and
/// surfaces drift rather than being silently skipped .
/// Exit codes:
/// - `0` — every bucket verified with no error-severity diagnostic.
/// - `1` — at least one runtime failure (pool / snapshot / verify error)
///   or at least one bucket reported an error-severity diagnostic.
/// - `2` — the server is below the minimum supported Postgres version
///   (a server-global refusal: verify returns immediately).
async fn run_verify(provider: &dyn DescriptorProvider, workspace: &Path, strict: bool) -> i32 {
    use djogi::config::DjogiConfig;

    // 0. Zero-descriptor refusal (§5.6 / REQ-370-8). `verify` refuses with
    // the dual-cause diagnostic + exit 2 ONLY when there are NEITHER
    // descriptors NOR on-disk snapshots — the genuinely unusable state
    // (a standalone binary with nothing to verify against). When
    // snapshots exist, verify DEGRADES to snapshot-only (the union below
    // enumerates the disk buckets), so we must not refuse here.
    // Guard on `provider.models().is_empty()` rather than the projected
    // `bucket_set`: projection always seeds the synthetic global bucket
    // (`(main, "")`), so the bucket set is never empty and is the wrong
    // signal for "no descriptors". This is the same guard the
    // compose/schema/docs gates in `lib.rs` use.
    if provider.models().is_empty() && discover_snapshot_buckets_on_disk(workspace).is_empty() {
        crate::print_zero_descriptor_diagnostic("migrations verify");
        return 2;
    }

    // 1. Load config from workspace.
    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations verify: config load: {e}");
            return 1;
        }
    };

    // 2. Project schema from descriptor provider.
    let models = match project_from_provider(provider) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("djogi migrations verify: projection error: {e}");
            return 1;
        }
    };

    // 3. Build the bucket set as the UNION of the inventory projection and
    // the on-disk snapshot tree . An orphaned snapshot
    // a removed app whose snapshot still sits on disk — is absent from
    // `models` but present on disk; without the union it would never be
    // verified and out-of-band drift would go unreported.
    let mut bucket_set: std::collections::BTreeSet<djogi::migrate::BucketKey> =
        models.keys().cloned().collect();
    for bucket in discover_snapshot_buckets_on_disk(workspace) {
        bucket_set.insert(bucket);
    }
    // The zero-descriptor refusal (step 0) already returned for the only
    // state that yields an empty bucket set (no descriptors + no snapshots).
    // Projection always seeds the synthetic global bucket, so reaching here
    // with an empty set is impossible; if a future projection change ever
    // breaks that invariant, fail closed with the dual-cause refusal rather
    // than silently reporting success on a binary that verified nothing.
    if bucket_set.is_empty() {
        crate::print_zero_descriptor_diagnostic("migrations verify");
        return 2;
    }

    // 4. Policy configuration for the --strict flag.
    let policy = djogi::config::PolicyConfig {
        strict_out_of_order: strict,
    };

    // 5. Pre-compute the set of databases that have at least one INVENTORY
    // bucket with non-empty models. Orphan-only databases (snapshots on
    // disk but no registered models) are excluded — `unwrap_or(false)`
    // treats a disk-only bucket as model-less. This gates D699 inside
    // `verify_bucket`: an orphan-only database has no live tables to
    // miss, so D601 is the actionable signal instead.
    let database_has_models: std::collections::HashSet<String> = bucket_set
        .iter()
        .filter(|b| {
            models
                .get(*b)
                .map(|s| !s.models.is_empty())
                .unwrap_or(false)
        })
        .map(|b| b.database.clone())
        .collect();

    // 6. Per-database context cache + dedup sets. Contexts are connected
    // lazily (only for databases that have a bucket needing a live read)
    // and reused across that database's app buckets. `seen_ledger_databases`
    // ensures the ledger-lifecycle diagnostics (D621/D622/D699) are
    // emitted once per database, not once per app bucket.
    let mut contexts: std::collections::BTreeMap<String, djogi::context::DjogiContext> =
        std::collections::BTreeMap::new();
    let mut seen_ledger_databases = std::collections::HashSet::<String>::new();
    let mut exit_code: i32 = 0;

    // 7. Verify each bucket.
    for bucket in &bucket_set {
        // a. Resolve the per-database URL.
        let Some(url) = resolve_bucket_url(&config.database, &bucket.database) else {
            let bd = if bucket.app.is_empty() {
                "_global_"
            } else {
                &bucket.app
            };
            eprintln!(
                "djogi migrations verify: cannot derive URL for database '{}' (bucket {}/{}); \
                 check that config.database.url has a valid path component",
                bucket.database, bucket.database, bd
            );
            exit_code = 1;
            continue;
        };

        // b. Connect (lazily, once per distinct database). PG < 18 is a
        // server-global refusal — there is no point continuing to other
        // buckets, so we return 2 immediately.
        if !contexts.contains_key(&bucket.database) {
            match connect_and_check(&url).await {
                ContextOutcome::Ready(ctx) => {
                    contexts.insert(bucket.database.clone(), ctx);
                }
                ContextOutcome::UnsupportedVersion(e) => {
                    crate::print_support_boundary_error("migrations verify", &e);
                    return 2;
                }
                ContextOutcome::RuntimeError(msg) => {
                    eprintln!(
                        "djogi migrations verify: pool for '{}': {msg}",
                        bucket.database
                    );
                    exit_code = 1;
                    continue;
                }
            }
        }

        // c. Load the snapshot. A missing snapshot for a bucket that HAS
        // registered models is a hard error (exit 1) — the operator must
        // record a baseline; a missing snapshot for a model-less bucket
        // is informational.
        let snap_path = snapshot_path(workspace, bucket);
        let snapshot = match load_snapshot(&snap_path) {
            Ok(s) => s,
            Err(SnapshotError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                let bd = if bucket.app.is_empty() {
                    "_global_"
                } else {
                    &bucket.app
                };
                let has_models = models
                    .get(bucket)
                    .map(|s| !s.models.is_empty())
                    .unwrap_or(false);
                if has_models {
                    eprintln!(
                        "djogi migrations verify: {}/{} has registered models but no \
                         snapshot; run `djogi migrations compose` then \
                         `djogi migrations apply` to record a baseline",
                        bucket.database, bd
                    );
                    exit_code = 1;
                } else {
                    println!("No snapshot found for bucket {}/{}", bucket.database, bd);
                }
                continue;
            }
            Err(e) => {
                let bd = if bucket.app.is_empty() {
                    "_global_"
                } else {
                    &bucket.app
                };
                eprintln!(
                    "djogi migrations verify: load snapshot for {}/{}: {e}",
                    bucket.database, bd
                );
                exit_code = 1;
                continue;
            }
        };

        // d. Compute ledger-emission flags. The ledger is shared per
        // database; emit its lifecycle diagnostics once per database
        // (the first bucket of each database that reaches this point),
        // and only for databases that actually have registered models.
        let db_has_models = database_has_models.contains(&bucket.database);
        let emit_ledger = db_has_models && seen_ledger_databases.insert(bucket.database.clone());

        // e. Run the bucket-scoped verify against the routed context.
        let ctx = contexts
            .get_mut(&bucket.database)
            .expect("context inserted above");
        let report = match djogi::migrate::verify_bucket(
            ctx,
            bucket,
            &snapshot,
            &policy,
            emit_ledger,
            db_has_models,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let bd = if bucket.app.is_empty() {
                    "_global_"
                } else {
                    &bucket.app
                };
                eprintln!(
                    "djogi migrations verify: error for {}/{}: {e}",
                    bucket.database, bd
                );
                exit_code = 1;
                continue;
            }
        };

        // f. Render and fold the bucket's error state into the exit code.
        for line in render_verify_report(&report, bucket) {
            println!("{line}");
        }
        if report.has_errors() {
            exit_code = 1;
        }
    }

    exit_code
}

/// Render a [`VerifyReport`] to a vector of output lines.
/// Format: one line per diagnostic with severity prefix, code, location,
/// and message. Summary line at the end. Output is deterministic because
/// `report.diagnostics` is already sorted by `(code, location)`.
/// Returns the lines instead of printing directly so the rendering is unit-
/// testable ; the caller iterates the returned vector and prints each
/// line. Blank separator lines are returned as empty strings.
fn render_verify_report(report: &VerifyReport, bucket: &BucketKey) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    let app_display = if bucket.app.is_empty() {
        "_global_"
    } else {
        &bucket.app
    };
    lines.push(format!(
        "djogi migrations verify — {}/{}",
        bucket.database, app_display
    ));
    lines.push("──────────────────────────────────────────".to_string());

    match (
        &report.latest_applied_version,
        report.applied_count,
        report.unfinished_count,
    ) {
        (Some(version), applied, 0) => {
            lines.push(format!("Ledger: {applied} applied, latest {version}"));
        }
        (Some(version), applied, unfinished) => {
            lines.push(format!(
                "Ledger: {applied} applied, {unfinished} unfinished, latest {version}"
            ));
        }
        (None, 0, 0) => {
            lines.push("Ledger: empty (no migrations applied yet)".to_string());
        }
        _ => {}
    }
    lines.push(String::new());

    if report.diagnostics.is_empty() {
        lines.push("No drift detected. Schema is consistent.".to_string());
    } else {
        for d in &report.diagnostics {
            let severity = match d.severity {
                VerifySeverity::Info => "INFO",
                VerifySeverity::Warning => "WARN",
                VerifySeverity::Error => "ERROR",
            };
            let location = d.location.as_deref().unwrap_or("-");
            lines.push(format!(
                "[{severity}] {code} ({loc}): {msg}",
                severity = severity,
                code = d.code,
                loc = location,
                msg = d.message
            ));
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
        lines.push(String::new());
        lines.push(format!(
            "Result: FAILED ({errors} error(s), {warnings} warning(s), {infos} info(s))"
        ));
    } else if warnings > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Result: PASSED with warnings ({warnings} warning(s), {infos} info(s))"
        ));
    } else {
        lines.push(String::new());
        lines.push(format!("Result: PASSED ({infos} info(s))"));
    }

    lines
}

// ── repair subcommand dispatch ────────────────────────────────────────────

impl From<PartialApplyResolutionCli> for PartialApplyResolution {
    fn from(cli: PartialApplyResolutionCli) -> Self {
        match cli {
            PartialApplyResolutionCli::RolledBack => Self::MarkRolledBack,
            PartialApplyResolutionCli::Faked => Self::MarkFaked,
            PartialApplyResolutionCli::Applied => Self::MarkApplied,
        }
    }
}

/// `djogi migrations repair <subcommand>` entry point.
/// Routes each subcommand to its glue function. The glue functions own
/// the runtime / config / pool / lock / report-render lifecycle; this
/// router only destructures the parsed clap variant.
pub fn repair_cmd(command: RepairSubcommand) -> ExitCode {
    match command {
        RepairSubcommand::ChecksumDrift {
            version,
            app,
            database,
            checksum_up,
            checksum_down,
            workspace,
        } => repair_checksum_drift_cmd(
            &version,
            app.as_deref(),
            database.as_deref(),
            checksum_up.as_deref(),
            checksum_down.as_deref(),
            workspace,
        ),
        RepairSubcommand::PartialApply {
            version,
            resolution,
            note,
            app,
            database,
            workspace,
        } => repair_partial_apply_cmd(
            &version,
            resolution.into(),
            &note,
            app.as_deref(),
            database.as_deref(),
            workspace,
        ),
        RepairSubcommand::ResumePartial {
            version,
            app,
            database,
            workspace,
            node_id,
            single_node_dev,
        } => repair_resume_partial_apply_cmd(
            &version,
            app.as_deref(),
            database.as_deref(),
            workspace,
            node_id,
            single_node_dev,
        ),
        RepairSubcommand::SnapshotRebuild {
            app,
            database,
            snapshot_path,
            workspace,
        } => repair_snapshot_rebuild_cmd(
            app.as_deref(),
            database.as_deref(),
            snapshot_path.as_deref(),
            workspace,
        ),
    }
}

/// Render a [`RepairReport`] to stdout. Shared across all four repair
/// glue functions so the operator sees a consistent action / ledger /
/// snapshot summary regardless of which repair ran.
fn render_repair_report(report: &RepairReport) {
    for action in &report.actions_taken {
        println!("  {action}");
    }
    if !report.ledger_changes.is_empty() {
        println!("Ledger changes:");
        for lc in &report.ledger_changes {
            println!(
                "  {} | {} | {} -> {}",
                lc.version, lc.column, lc.before, lc.after,
            );
        }
    }
    if !report.snapshot_changes.is_empty() {
        println!("Snapshot changes:");
        for sc in &report.snapshot_changes {
            println!("  {} | {}", sc.path.display(), sc.description);
        }
    }
}

/// Map a [`RepairError`] onto the CLI exit-code contract.
/// `RepairError` is NOT `#[non_exhaustive]`, so this match is
/// **exhaustive with NO `_ =>` wildcard** by deliberate design: a future
/// variant breaks compilation here, forcing a conscious exit-code
/// classification rather than silently bucketing an unclassified error.
/// Classification rule — when a new variant is added, classify it the
/// same way:
/// - **Exit 1 (retryable):** variants wrapping a transient I/O /
///   connection / pool / SQL failure (a `source: DjogiError`, snapshot
///   filesystem I/O, or advisory-lock contention). A retry may succeed.
/// - **Exit 2 (refusal):** structural refusals and ledger-logic guards
///   that require operator intervention. A blind retry hits the same
///   refusal.
fn repair_error_exit_code(err: &RepairError) -> i32 {
    match err {
        // ── Exit 1: transient I/O / connection / pool / SQL failures.
        // These wrap a DjogiError (network, connection, query) or a
        // filesystem error and may succeed on retry.
        RepairError::LedgerIo { .. }                  // ledger DB I/O
        | RepairError::SnapshotIo { .. }              // snapshot filesystem I/O
        | RepairError::AdvisoryLockFailed { .. }      // lock held by a concurrent runner; retry after it releases
        | RepairError::AdvisoryLockQueryFailed { .. } // pg_try_advisory_lock query itself errored
        | RepairError::PinnedSessionCheckoutFailed { .. } // could not check out a pinned session from the pool
        | RepairError::ResumeStepFailed { .. }        // a replayed statement failed; partial state recorded, retryable
        | RepairError::ResumeProgressAckFailed { .. } // step committed but the progress ack write failed; retryable
        => 1,

        // ── Exit 2: refusals and structural / ledger-logic guards.
        // The operator must investigate and intervene; a blind retry
        // would hit the same refusal.
        RepairError::VersionNotFound { .. }
        | RepairError::InsufficientConfirmation
        | RepairError::InvalidChecksum { .. }
        | RepairError::InvalidResolution { .. }
        | RepairError::BucketAppMismatch { .. }
        | RepairError::PlanVersionMismatch { .. }
        | RepairError::PlanChecksumMismatch { .. }
        | RepairError::LeafIdentityMismatch { .. }
        | RepairError::NothingToResume { .. }
        | RepairError::ResumeBlockedByNonTxProgressClaim { .. }
        | RepairError::SuppliedSnapshotDiverges { .. }
        | RepairError::AdvisoryUnlockReturnedFalse { .. } // session-pinning correctness failure — not a blind retry
        | RepairError::ResumePlanShapeMismatch { .. }
        | RepairError::ReplayPlanShapeMismatch { .. }
        | RepairError::PhaseZeroArtifactRefused { .. }  // #386: refusal — operator must replace the stale file
        | RepairError::MissingResumeIdentity { .. }     // #386: refusal — operator must supply identity for resume
        => 2,
    }
}

/// Resolve the database name for bucket construction. Uses the explicit
/// `--database` flag if provided, otherwise defaults to `"main"` (the
/// global database name — see [`djogi::apps::AppDescriptor::GLOBAL_DATABASE`]).
/// `_config` is threaded so this single helper can grow a config-driven
/// default database (should `DjogiConfig` gain one) without changing
/// every call site.
fn resolve_database(database: Option<&str>, _config: &djogi::config::DjogiConfig) -> String {
    database.unwrap_or("main").to_string()
}

/// Compute the `V1:`-prefixed checksum of a committed up SQL file on disk,
/// using the canonical fragment-level domain (strips the composed-file
/// header and label comments, matching what compose stores in the ledger).
/// The naive whole-file checksum is WRONG here: compose stores checksums
/// computed over the [`djogi::migrate::OperationSql`] fragments only,
/// without the rendered file's `-- Djogi composed migration — up` header
/// or the per-statement label comment lines. Recomputing over the full
/// file content would never match the ledger value, so the drift repair
/// would write a checksum that immediately re-drifts. Delegating to
/// [`djogi::migrate::compute_committed_sql_checksum`] keeps the CLI's
/// recompute path in the same domain as compose.
/// Returns the underlying [`std::io::Error`] unchanged so the caller can
/// surface a missing/unreadable up file as a retryable I/O error.
fn compute_checksum_up_from_disk(
    workspace: &Path,
    bucket: &djogi::migrate::BucketKey,
    version: &str,
) -> std::io::Result<String> {
    let path =
        djogi::migrate::bucket_dir(workspace, bucket).join(djogi::migrate::up_filename(version));
    let sql = std::fs::read_to_string(&path)?;
    Ok(djogi::migrate::compute_committed_sql_checksum(
        &sql,
        djogi::migrate::ResetSqlSide::Up,
    ))
}

/// Compute the canonical checksum of a committed down SQL file on disk,
/// using the same fragment-level domain as compose (see
/// [`compute_checksum_up_from_disk`] for why the whole-file checksum is
/// wrong).
/// Returns `Ok(None)` when the file is absent
/// ([`std::io::ErrorKind::NotFound`]) or when the file contains only SQL
/// comments — both map onto compose's `NULL` `checksum_down` sentinel for
/// comment-only down files. Returns `Err` for any other I/O failure so a
/// retry after the file is restored can succeed.
fn compute_checksum_down_from_disk(
    workspace: &Path,
    bucket: &djogi::migrate::BucketKey,
    version: &str,
) -> std::io::Result<Option<String>> {
    let path =
        djogi::migrate::bucket_dir(workspace, bucket).join(djogi::migrate::down_filename(version));
    let sql = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(djogi::migrate::compute_committed_down_sql_checksum(&sql))
}

/// `djogi migrations repair checksum-drift` entry point.
/// Updates the `checksum_up` / `checksum_down` columns of an
/// already-applied ledger row after its committed SQL was edited. When
/// `--checksum-up` / `--checksum-down` are omitted, the checksums are
/// recomputed from the committed files on disk (a missing down file is a
/// no-op; any other read error aborts with exit 1).
pub fn repair_checksum_drift_cmd(
    version: &str,
    app: Option<&str>,
    database: Option<&str>,
    checksum_up: Option<&str>,
    checksum_down: Option<&str>,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations repair checksum-drift: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async {
        run_repair_checksum_drift(
            &workspace,
            version,
            app,
            database,
            checksum_up,
            checksum_down,
        )
        .await
    });
    ExitCode::from(exit as u8)
}

/// Async body of [`repair_checksum_drift_cmd`]. Returns the desired exit code.
async fn run_repair_checksum_drift(
    workspace: &Path,
    version: &str,
    app: Option<&str>,
    database: Option<&str>,
    checksum_up: Option<&str>,
    checksum_down: Option<&str>,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations repair checksum-drift: config load: {e}");
            return 1;
        }
    };

    // Resolve the per-database URL BEFORE connecting: `--database
    // crud_log` / `event_log` operate on a different bucket's ledger than
    // the app DB, so connecting to `config.database.url` first would
    // silently mutate the wrong database.
    let db_name = resolve_database(database, &config);
    let url = match resolve_bucket_url(&config.database, &db_name) {
        Some(u) => u,
        None => {
            eprintln!(
                "djogi migrations repair checksum-drift: cannot derive a database URL for `{db_name}`"
            );
            return 2;
        }
    };

    let mut ctx = match connect_and_check(&url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations repair checksum-drift", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations repair checksum-drift: pool: {msg}");
            return 1;
        }
    };

    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations repair checksum-drift: workspace lock: {e}");
            return 1;
        }
    };

    let app_label = app.unwrap_or("");
    let bucket = BucketKey {
        database: db_name,
        app: app_label.to_string(),
    };

    let new_checksum_up = match checksum_up {
        Some(c) => c.to_string(),
        None => {
            // Auto-compute from the committed up SQL file on disk. A
            // missing or unreadable up file is an environment I/O error,
            // not an operator-facing refusal — exit 1 (same class as the
            // down file's non-NotFound branch below), so a retry after
            // the file is restored can succeed.
            match compute_checksum_up_from_disk(workspace, &bucket, version) {
                Ok(cs) => cs,
                Err(e) => {
                    eprintln!("djogi migrations repair checksum-drift: compute checksum_up: {e}");
                    return 1;
                }
            }
        }
    };

    let resolved_checksum_down = match checksum_down {
        Some(c) => Some(c.to_string()),
        None => {
            // Auto-compute from the down file; a missing down file (or a
            // comment-only down file) is a no-op (no down checksum), other
            // read errors surface. NotFound is folded into `Ok(None)` by
            // `compute_checksum_down_from_disk`.
            match compute_checksum_down_from_disk(workspace, &bucket, version) {
                Ok(cs_opt) => cs_opt,
                Err(e) => {
                    eprintln!("djogi migrations repair checksum-drift: read down SQL: {e}");
                    return 1;
                }
            }
        }
    };

    match repair_checksum_drift(
        &mut ctx,
        &guard,
        &bucket,
        version,
        workspace,
        &new_checksum_up,
        resolved_checksum_down.as_deref(),
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    {
        Ok(report) => {
            render_repair_report(&report);
            0
        }
        Err(e) => {
            eprintln!("djogi migrations repair checksum-drift: {e}");
            repair_error_exit_code(&e)
        }
    }
}

/// `djogi migrations repair partial-apply` entry point.
/// Resolves a partial-apply ledger row by rewriting its status to
/// `rolled_back`, `faked`, or `applied`. No SQL executes — only the
/// ledger row is mutated.
pub fn repair_partial_apply_cmd(
    version: &str,
    resolution: PartialApplyResolution,
    note: &str,
    app: Option<&str>,
    database: Option<&str>,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations repair partial-apply: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async {
        run_repair_partial_apply(&workspace, version, resolution, note, app, database).await
    });
    ExitCode::from(exit as u8)
}

/// Async body of [`repair_partial_apply_cmd`]. Returns the desired exit code.
async fn run_repair_partial_apply(
    workspace: &Path,
    version: &str,
    resolution: PartialApplyResolution,
    note: &str,
    app: Option<&str>,
    database: Option<&str>,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations repair partial-apply: config load: {e}");
            return 1;
        }
    };

    // Resolve the per-database URL BEFORE connecting: `--database
    // crud_log` / `event_log` operate on a different bucket's ledger than
    // the app DB, so connecting to `config.database.url` first would
    // silently mutate the wrong database.
    let db_name = resolve_database(database, &config);
    let url = match resolve_bucket_url(&config.database, &db_name) {
        Some(u) => u,
        None => {
            eprintln!(
                "djogi migrations repair partial-apply: cannot derive a database URL for `{db_name}`"
            );
            return 2;
        }
    };

    let mut ctx = match connect_and_check(&url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations repair partial-apply", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations repair partial-apply: pool: {msg}");
            return 1;
        }
    };

    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations repair partial-apply: workspace lock: {e}");
            return 1;
        }
    };

    let app_label = app.unwrap_or("");
    let bucket = BucketKey {
        database: db_name,
        app: app_label.to_string(),
    };

    match repair_partial_apply(
        &mut ctx,
        &guard,
        &bucket,
        version,
        workspace,
        resolution,
        note,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    {
        Ok(report) => {
            render_repair_report(&report);
            0
        }
        Err(e) => {
            eprintln!("djogi migrations repair partial-apply: {e}");
            repair_error_exit_code(&e)
        }
    }
}

/// `djogi migrations repair resume-partial` entry point.
/// Resumes an interrupted non-transactional apply by loading the
/// committed `<version>.plan.json` and replaying its remaining steps.
/// Loads the committed plan directly (no CLI-level checksum pre-gate);
/// the library validates the plan against the ledger row internally.
pub fn repair_resume_partial_apply_cmd(
    version: &str,
    app: Option<&str>,
    database: Option<&str>,
    workspace: Option<PathBuf>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations repair resume-partial: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async {
        run_repair_resume_partial(&workspace, version, app, database, node_id, single_node_dev)
            .await
    });
    ExitCode::from(exit as u8)
}

/// Async body of [`repair_resume_partial_apply_cmd`]. Returns the desired exit code.
async fn run_repair_resume_partial(
    workspace: &Path,
    version: &str,
    app: Option<&str>,
    database: Option<&str>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations repair resume-partial: config load: {e}");
            return 1;
        }
    };

    // Resolve node identity before any DB work.
    let runner_identity = match crate::identity::resolve_identity(
        node_id,
        single_node_dev,
        &config.profile,
        "repair resume-partial",
    ) {
        Ok(resolved) => Some(resolved.into_runner_identity()),
        Err(e) => {
            let _ = crate::identity::print_identity_error("repair resume-partial", &e);
            return 2;
        }
    };

    // Resolve the per-database URL BEFORE connecting: `--database
    // crud_log` / `event_log` operate on a different bucket's ledger than
    // the app DB, so connecting to `config.database.url` first would
    // silently mutate the wrong database.
    let db_name = resolve_database(database, &config);
    let url = match resolve_bucket_url(&config.database, &db_name) {
        Some(u) => u,
        None => {
            eprintln!(
                "djogi migrations repair resume-partial: cannot derive a database URL for `{db_name}`"
            );
            return 2;
        }
    };

    let mut ctx = match connect_and_check(&url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations repair resume-partial", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations repair resume-partial: pool: {msg}");
            return 1;
        }
    };

    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations repair resume-partial: workspace lock: {e}");
            return 1;
        }
    };

    let app_label = app.unwrap_or("");
    let bucket = BucketKey {
        database: db_name,
        app: app_label.to_string(),
    };

    // Load the committed replay plan directly from disk — no CLI-level
    // checksum pre-gate, because repair_resume_partial_apply validates
    // plan↔ledger checksums itself. Synthesizing a whole-file checksum
    // here would not match the per-statement-fragment checksums stored
    // in the plan JSON.
    let plan = match load_committed_plan_for_resume(workspace, &bucket, version) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("djogi migrations repair resume-partial: load plan: {e}");
            return 2;
        }
    };

    match repair_resume_partial_apply(
        &mut ctx,
        &guard,
        workspace,
        version,
        &plan,
        runner_identity,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    {
        Ok(report) => {
            render_repair_report(&report);
            0
        }
        Err(e) => {
            eprintln!("djogi migrations repair resume-partial: {e}");
            repair_error_exit_code(&e)
        }
    }
}

/// Load the committed `<version>.plan.json` for `resume-partial` without
/// the CLI-level checksum pre-gate.
/// [`repair_resume_partial_apply`] validates the plan against the ledger
/// row internally (`PlanVersionMismatch` / `PlanChecksumMismatch`), so
/// re-gating here with a hand-rolled whole-file checksum would be both
/// wrong (the plan stores per-statement-fragment checksums) and
/// redundant. This helper therefore deliberately does NOT reuse
/// [`load_replay_plan_from_disk`] (a pending-apply helper that DOES
/// checksum-gate) — it reuses only that function's `CliReplay*`
/// deserialization + segment-conversion shape.
/// Returns a human-readable error string on a missing/unparseable plan
/// file or a format-version mismatch. A missing plan file maps to exit 2
/// at the call site (the committed plan is a precondition of resume).
fn load_committed_plan_for_resume(
    workspace: &Path,
    bucket: &djogi::migrate::BucketKey,
    version: &str,
) -> Result<djogi::migrate::MigrationPlan, String> {
    let bucket_dir = djogi::migrate::bucket_dir(workspace, bucket);
    let plan_path = bucket_dir.join(format!("{version}.plan.json"));
    let bytes = std::fs::read(&plan_path).map_err(|e| format!("{}: {e}", plan_path.display()))?;
    let stored: CliReplayPlan = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{}: parse: {e}", plan_path.display()))?;
    if stored.format_version != CLI_REPLAY_PLAN_FORMAT_VERSION {
        return Err(format!(
            "{}: unsupported format version {} (expected {CLI_REPLAY_PLAN_FORMAT_VERSION})",
            plan_path.display(),
            stored.format_version,
        ));
    }
    Ok(djogi::migrate::MigrationPlan {
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
    })
}

/// `djogi migrations repair snapshot-rebuild` entry point.
/// Rebuilds a bucket's schema snapshot by walking the ledger and
/// re-projecting from live database state. When `--snapshot-path` is
/// omitted, the path is derived from
/// `migrations/<database>/<app>/schema_snapshot.json`.
pub fn repair_snapshot_rebuild_cmd(
    app: Option<&str>,
    database: Option<&str>,
    snapshot_path: Option<&Path>,
    workspace: Option<PathBuf>,
) -> ExitCode {
    let workspace = resolve_workspace(workspace);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations repair snapshot-rebuild: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async {
        run_repair_snapshot_rebuild(&workspace, app, database, snapshot_path).await
    });
    ExitCode::from(exit as u8)
}

/// Async body of [`repair_snapshot_rebuild_cmd`]. Returns the desired exit code.
async fn run_repair_snapshot_rebuild(
    workspace: &Path,
    app: Option<&str>,
    database: Option<&str>,
    snapshot_path: Option<&Path>,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations repair snapshot-rebuild: config load: {e}");
            return 1;
        }
    };

    // Resolve the per-database URL BEFORE connecting: `--database
    // crud_log` / `event_log` operate on a different bucket's ledger than
    // the app DB, so connecting to `config.database.url` first would
    // silently rebuild the snapshot from the wrong database.
    let db_name = resolve_database(database, &config);
    let url = match resolve_bucket_url(&config.database, &db_name) {
        Some(u) => u,
        None => {
            eprintln!(
                "djogi migrations repair snapshot-rebuild: cannot derive a database URL for `{db_name}`"
            );
            return 2;
        }
    };

    let mut ctx = match connect_and_check(&url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations repair snapshot-rebuild", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations repair snapshot-rebuild: pool: {msg}");
            return 1;
        }
    };

    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations repair snapshot-rebuild: workspace lock: {e}");
            return 1;
        }
    };

    let app_label = app.unwrap_or("");
    let bucket = BucketKey {
        database: db_name,
        app: app_label.to_string(),
    };

    let snap_path = match snapshot_path {
        Some(p) => p.to_path_buf(),
        None => reconstruct_snapshot_path(workspace, &bucket),
    };

    match repair_snapshot_rebuild(
        &mut ctx,
        &guard,
        &bucket,
        &snap_path,
        RepairConfirmation::OperatorAcknowledged,
    )
    .await
    {
        Ok(report) => {
            render_repair_report(&report);
            0
        }
        Err(e) => {
            eprintln!("djogi migrations repair snapshot-rebuild: {e}");
            repair_error_exit_code(&e)
        }
    }
}

// ── baseline command ──────────────────────────────────────────────────────

/// `djogi migrations baseline` entry point.
/// Establishes a baseline ledger row + snapshot for an existing
/// database adopted under Djogi's migration ledger. The schema already
/// exists, so `compose` + `apply` cannot run against the populated
/// database without a starting point; baseline projects the live
/// catalog into a single `baseline` ledger row (no SQL runs against
/// user tables) and persists the projected snapshot as the canonical
/// baseline so future migrations diff against the real DB state.
/// `--reason` is required and must be non-empty — it is recorded in the
/// ledger row's `partial_apply_note` for the audit trail. An empty
/// reason is a refusal (exit 2) caught before any DB work.
/// Exit codes: `0` success, `1` runtime error (config / pool / projection
/// failure), `2` refusal (empty `--reason`, unresolvable database URL,
/// duplicate version collision, snapshot-persist failure after ledger
/// insert, session-pinning correctness failure, or below PG 18).
#[expect(
    clippy::too_many_arguments,
    reason = "CLI command entry point mirrors clap arguments explicitly"
)]
pub fn baseline_cmd(
    version: &str,
    description: &str,
    reason: &str,
    app: Option<&str>,
    database: Option<&str>,
    workspace: Option<PathBuf>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> ExitCode {
    // Validate --reason before any expensive work, mirroring the
    // `apply --fake --reason` empty-reason gate. The library's
    // baseline_plan does not itself reject an empty reason (it records
    // whatever string it is handed), so the CLI owns this guard.
    if reason.trim().is_empty() {
        eprintln!(
            "djogi migrations baseline: --reason must not be empty; \
             supply a non-empty reason why this baseline is being established \
             (e.g. 'schema pre-exists from prior tooling'). \
             This is recorded in the ledger audit trail."
        );
        return ExitCode::from(2);
    }

    let workspace = resolve_workspace(workspace);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations baseline: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async {
        run_baseline(
            &workspace,
            version,
            description,
            reason,
            app,
            database,
            node_id,
            single_node_dev,
        )
        .await
    });
    ExitCode::from(exit as u8)
}

/// Async body of [`baseline_cmd`]. Returns the desired exit code.
/// Resolves the per-database URL BEFORE connecting (a `--database
/// crud_log` / `event_log` baseline targets a different bucket's ledger
/// than the app DB), connects + runs the PG-version preflight via
/// [`connect_and_check`], acquires the workspace file lock, then drives
/// [`baseline_plan`]. The runner projects the live schema itself and
/// computes the baseline checksum from that projection, so the
/// `RunnerCtx` is constructed with `snapshot: None` (requires the
/// caller NOT supply a snapshot) and an empty `checksum_up` (the
/// baseline path never reads it).
#[expect(
    clippy::too_many_arguments,
    reason = "baseline async body keeps CLI arguments explicit through validation and connection setup"
)]
async fn run_baseline(
    workspace: &Path,
    version: &str,
    description: &str,
    reason: &str,
    app: Option<&str>,
    database: Option<&str>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi migrations baseline: config load: {e}");
            return 1;
        }
    };

    // Resolve node identity before any DB work.
    let runner_identity = match crate::identity::resolve_identity(
        node_id,
        single_node_dev,
        &config.profile,
        "baseline",
    ) {
        Ok(resolved) => Some(resolved.into_runner_identity()),
        Err(e) => {
            let _ = crate::identity::print_identity_error("baseline", &e);
            return 2;
        }
    };

    // Resolve the per-database URL BEFORE connecting: `--database
    // crud_log` / `event_log` operate on a different bucket's ledger
    // than the app DB, so connecting to `config.database.url` first
    // would silently baseline the wrong database.
    let db_name = resolve_database(database, &config);
    let url = match resolve_bucket_url(&config.database, &db_name) {
        Some(u) => u,
        None => {
            eprintln!("djogi migrations baseline: cannot derive a database URL for `{db_name}`");
            return 2;
        }
    };

    let mut ctx = match connect_and_check(&url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations baseline", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations baseline: pool: {msg}");
            return 1;
        }
    };

    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("djogi migrations baseline: workspace lock: {e}");
            return 1;
        }
    };

    let app_label = app.unwrap_or("");
    let bucket = BucketKey {
        database: db_name,
        app: app_label.to_string(),
    };

    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: version.to_string(),
        description: description.to_string(),
        // baseline_plan computes checksum_up from the live projection;
        // this field is not read on the baseline code path.
        checksum_up: String::new(),
        checksum_down: None,
        // baseline_plan refuses a caller-supplied snapshot — it
        // projects the live DB itself. Leave this None; the projection
        // is persisted to `snapshot_path` below.
        snapshot: None,
        snapshot_path: Some(reconstruct_snapshot_path(workspace, &bucket)),
        // MigrateConfig does not derive Clone; construct from fields
        // (same pattern as apply_one_pending).
        config: djogi::config::MigrateConfig {
            concurrent_warn_relpages: config.migrate.concurrent_warn_relpages,
            strict_concurrent_warnings: config.migrate.strict_concurrent_warnings,
            pk_flip_long_tx_threshold_secs: config.migrate.pk_flip_long_tx_threshold_secs,
            pk_flip_join_table_option: config.migrate.pk_flip_join_table_option,
        },
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::default_for_config(&config),
        audit_pool: match djogi::migrate::resolve_audit_url(&config) {
            Ok(url) => djogi::migrate::build_audit_pool(&url).await.ok(),
            Err(_) => None,
        },
        runner_identity,
    };

    match baseline_plan(&mut ctx, &bucket, &runner_ctx, &guard, reason).await {
        Ok(report) => {
            println!(
                "djogi migrations baseline: established baseline `{}` \
                 (ledger_id={}) in {:.1}s",
                version,
                report.ledger_id,
                report.execution_time_ms as f64 / 1000.0
            );
            0
        }
        Err(e) => {
            eprintln!("djogi migrations baseline: {e}");
            baseline_error_exit_code(&e)
        }
    }
}

/// Map a [`RunnerError`] produced by [`baseline_plan`] onto the CLI
/// exit-code contract.
/// The flat [`runner_error_exit_code`] (always `1`) is wrong for
/// baseline: a duplicate-version collision is a refusal the operator
/// must resolve by choosing a new version, and a blind retry hits the
/// same collision — that must surface as exit `2`, matching the
/// `migrations apply` doc-contract ("re-running reports
/// `VersionAlreadyApplied` (exit 2)") and the `repair` family's
/// [`repair_error_exit_code`] convention.
/// `RunnerError` is `#[non_exhaustive]`, so the wildcard arm is
/// load-bearing: any variant NOT named below defaults to exit `1`
/// (transient — a retry may succeed). That is the safe default for the
/// I/O- and connection-shaped variants the baseline path can hit
/// (projection failure, ledger bootstrap / write / query failure,
/// snapshot persist failure, pinned-session checkout failure,
/// advisory-lock contention). Only the genuine refusals are pulled out
/// into the exit-`2` arm.
fn baseline_error_exit_code(err: &RunnerError) -> i32 {
    match err {
        // ── Exit 2: refusals — the operator must intervene; a blind
        // retry hits the same condition.
        // - A duplicate version (terminal or non-terminal) means the
        // chosen baseline version is already taken; pick another.
        // - A caller-supplied snapshot is a programming error in the
        // wiring (the CLI always passes `snapshot: None`), surfaced
        // as a structural refusal rather than a retryable fault.
        // - An out-of-order rejection is a policy refusal identical to
        // the apply path's.
        // - AdvisoryUnlockReturnedFalse is a session-pinning correctness
        // failure (PG returned false for pg_advisory_unlock); it is not
        // transient — matches the repair family's exit-2 treatment.
        // - SnapshotPersistFailed in the baseline path is a post-ledger
        // failure: baseline_inner inserts the terminal ledger row BEFORE
        // writing the snapshot. A retry with the same version therefore
        // hits VersionAlreadyApplied (exit 2) before it can write the
        // snapshot. Exit 1 (retryable) would give false hope; exit 2
        // signals that operator intervention is needed (run
        // `repair snapshot-rebuild` or choose a new version).
        RunnerError::VersionAlreadyApplied { .. }
        | RunnerError::VersionCollisionNonTerminal { .. }
        | RunnerError::BaselineSnapshotShouldNotBeProvided
        | RunnerError::AdvisoryUnlockReturnedFalse { .. }
        | RunnerError::SnapshotPersistFailed { .. }
        | RunnerError::OutOfOrderRejected { .. } => 2,
        // ── Exit 1: everything else (transient I/O / connection / SQL /
        // projection failures). `#[non_exhaustive]` makes this wildcard
        // mandatory; new transient-shaped variants inherit the retryable
        // default.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djogi::__bypass::RawAccessExt as _;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DatabaseUrlEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Option<String>,
    }

    impl DatabaseUrlEnvGuard {
        fn new() -> Self {
            Self {
                _lock: crate::test_env_lock(),
                prior: std::env::var("DATABASE_URL").ok(),
            }
        }

        fn set(&self, value: &str) {
            unsafe { std::env::set_var("DATABASE_URL", value) };
        }

        fn remove(&self) {
            unsafe { std::env::remove_var("DATABASE_URL") };
        }
    }

    impl Drop for DatabaseUrlEnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(value) => unsafe { std::env::set_var("DATABASE_URL", value) },
                None => unsafe { std::env::remove_var("DATABASE_URL") },
            }
        }
    }

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

    fn write_unreachable_config(work: &std::path::Path) {
        let toml = "[database]\nurl = \"postgres://localhost:1/djogi_unreachable\"\n\
                    max_connections = 1\ndev_mode = false\n\
                    [server]\nhost = \"127.0.0.1\"\nport = 1234\n";
        fs::write(work.join("Djogi.toml"), toml).unwrap();
    }

    fn without_database_url<T>(f: impl FnOnce() -> T) -> T {
        let env_guard = DatabaseUrlEnvGuard::new();
        env_guard.remove();
        f()
    }

    #[test]
    fn database_url_env_guard_restores_prior_value() {
        let env_guard = DatabaseUrlEnvGuard::new();
        let expected = env_guard.prior.clone();
        let next = if expected.as_deref() == Some("postgres://from-env/test") {
            "postgres://temporary/test"
        } else {
            "postgres://from-env/test"
        };
        env_guard.set(next);
        drop(env_guard);
        assert_eq!(std::env::var("DATABASE_URL").ok(), expected);
    }

    fn current_production_phase_zero_sql(tag: &str) -> String {
        let work = temp_workspace(tag);
        let lock_path = work.join(LOCK_FILE_NAME);
        let guard = acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT).expect("lock");
        let models: std::collections::BTreeMap<
            djogi::migrate::BucketKey,
            djogi::migrate::AppliedSchema,
        > = std::collections::BTreeMap::new();
        let apps = vec![AppLifecycle {
            label: "billing".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];
        let emitted = djogi::migrate::ensure_phase_zero_emitted(
            &work,
            &models,
            &apps,
            time::OffsetDateTime::now_utc(),
            &guard,
        )
        .expect("auto-emit Phase 0");
        let sql = fs::read_to_string(&emitted[0].up_sql_path).expect("read emitted Phase 0");
        drop(guard);
        let _ = fs::remove_dir_all(&work);
        sql
    }

    fn markerless_seed_phase_zero_sql(tag: &str) -> String {
        let mut sql = current_production_phase_zero_sql(tag);
        sql.push_str("\nINSERT INTO heer.heer_nodes (id) VALUES (1);\n");
        sql
    }

    fn phase_zero_with_seed_statement(tag: &str, statement: &str) -> String {
        let mut sql = current_production_phase_zero_sql(tag);
        sql.push('\n');
        sql.push_str(statement);
        sql.push('\n');
        sql
    }

    fn extended_seed_statement_cases() -> [(&'static str, &'static str); 4] {
        [
            (
                "cte_insert",
                "WITH rows AS (SELECT 1) INSERT INTO heer.heer_nodes (id) VALUES (1);",
            ),
            (
                "cte_delete",
                "WITH moved AS (DELETE FROM heer.heer_node_state RETURNING *) SELECT 1;",
            ),
            (
                "merge",
                "MERGE INTO heer.heer_nodes AS target USING incoming ON false WHEN NOT MATCHED THEN INSERT (id) VALUES (1);",
            ),
            (
                "copy_from",
                "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM STDIN;",
            ),
        ]
    }

    fn generated_stale_phase_zero_sql(tag: &str) -> String {
        let mut sql = current_production_phase_zero_sql(tag);
        sql.push_str(
            "\nALTER DATABASE \"mydb\" SET heer.node_id = '1';\n\
             ALTER DATABASE \"mydb\" SET heer.ranj_node_id = '1';\n\
             SET heer.node_id = '1';\n\
             SET heer.ranj_node_id = '1';\n",
        );
        sql
    }

    fn seed_capable_phase_zero_sql() -> String {
        djogi::testing::phase_zero_sql_for_testing("main", true)
            .expect("compose seed-capable Phase 0")
    }

    fn write_pending_json(
        path: &Path,
        database: &str,
        app: &str,
        version: &str,
        depends_on: &[&str],
    ) {
        let pending = PendingPlan {
            format_version: djogi::migrate::PENDING_FORMAT_VERSION.to_string(),
            bucket_database: database.to_string(),
            bucket_app: app.to_string(),
            version: version.to_string(),
            slug: "test".to_string(),
            model_snapshot: djogi::migrate::AppliedSchema {
                djogi_version: "0.1.0".to_string(),
                enums: std::collections::BTreeMap::new(),
                format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-06-06T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models: std::collections::BTreeMap::new(),
                registered_apps: vec![app.to_string()],
            },
            checksum_up: "V1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            checksum_down: None,
            composed_at: "2026-06-06T00:00:00Z".to_string(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_vec_pretty(&pending).unwrap()).unwrap();
    }

    /// The CLI's bucket-discovery walk must include directories that exist
    /// on disk but are absent from the current model inventory (the
    /// renamed-from case).
    #[test]
    fn b1_discover_snapshot_buckets_picks_up_renamed_from_app() {
        let work = temp_workspace("b1_discover");
        // Lay down a `migrations/main/billing/schema_snapshot.json`
        // the OLD app's snapshot. The current model inventory
        // would NOT have this bucket because the app moved to
        // `invoicing` via `#[app(renamed_from = "billing")]`.
        let billing_dir = work.join("migrations/main/billing");
        fs::create_dir_all(&billing_dir).unwrap();
        fs::write(billing_dir.join("schema_snapshot.json"), "{}").unwrap();
        // A second bucket — the global one for the same database
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

    /// The resolved workspace flows into config loading.
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
        let env_guard = DatabaseUrlEnvGuard::new();
        env_guard.remove();
        let config = djogi::config::DjogiConfig::load_from_workspace(&work).expect("load");
        assert_eq!(
            config.database.url,
            "postgres://discovered-by-workspace-flag/test"
        );
        assert_eq!(config.server.port, 1234);
        let _ = fs::remove_dir_all(&work);
    }

    /// Env override precedence: A `DATABASE_URL` in the environment
    /// must beat any value in
    /// `<workspace>/Djogi.toml`, matching the security contract that
    /// secrets only live in env vars.
    #[test]
    fn a1_round2_env_override_beats_workspace_toml() {
        let work = temp_workspace("a1r2_env_override");
        let toml = "[database]\nurl = \"postgres://from-toml/test\"\n\
                    max_connections = 1\ndev_mode = false\n\
                    [server]\nhost = \"127.0.0.1\"\nport = 1234\n";
        fs::write(work.join("Djogi.toml"), toml).unwrap();
        let env_guard = DatabaseUrlEnvGuard::new();
        env_guard.set("postgres://from-env/test");
        let config = djogi::config::DjogiConfig::load_from_workspace(&work).expect("load");
        assert_eq!(
            config.database.url, "postgres://from-env/test",
            "env DATABASE_URL must win over workspace Djogi.toml"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn apply_no_pending_is_identity_free_and_skips_pool_connect() {
        let work = temp_workspace("apply_no_pending");
        write_unreachable_config(&work);

        let exit = without_database_url(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(run_apply(&work, &FakeMode::Real, None, false))
        });

        assert_eq!(
            exit, 0,
            "no-pending apply must return before identity resolution or pool checkout"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_pending_plans_orders_phase_zero_before_normal_global() {
        let work = temp_workspace("discover_pending_phase_zero_first");
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: String::new(),
                },
            ),
            "main",
            "",
            "V20260606010101__later_global",
            &[],
        );
        write_pending_json(
            &djogi::migrate::phase_zero_pending_json_path(
                &work,
                "main",
                djogi::migrate::PHASE_ZERO_VERSION,
            ),
            "main",
            "",
            djogi::migrate::PHASE_ZERO_VERSION,
            &[],
        );

        let discovered = discover_pending_plans(&work).expect("discover");
        assert_eq!(discovered.len(), 2);
        assert_eq!(
            discovered[0].plan.version,
            djogi::migrate::PHASE_ZERO_VERSION
        );
        assert!(discovered[0].is_phase_zero);
        assert_eq!(discovered[1].plan.version, "V20260606010101__later_global");
        let _ = fs::remove_dir_all(&work);
    }

    /// Same-version buckets order by recorded depends_on, not path order.
    /// `system` depends on `users`, so `users` must come first even though
    /// `system` sorts earlier alphabetically.
    #[test]
    fn discover_orders_same_version_buckets_by_depends_on() {
        let work = temp_workspace("discover_pending_depends_on");
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: "system".to_string(),
                },
            ),
            "main",
            "system",
            "V20260609000000__initial",
            &["users"],
        );
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: "users".to_string(),
                },
            ),
            "main",
            "users",
            "V20260609000000__initial",
            &[],
        );

        let plans = discover_pending_plans(&work).expect("discovers");
        let apps: Vec<&str> = plans.iter().map(|p| p.bucket.app.as_str()).collect();
        assert_eq!(apps, ["users", "system"]);
        let _ = fs::remove_dir_all(&work);
    }

    /// Buckets with no dependencies should be ordered alphabetically by app
    /// name as a deterministic tiebreak in Kahn's topological sort.
    #[test]
    fn discover_orders_no_dependency_buckets_alphabetically() {
        let work = temp_workspace("discover_pending_alpha_tiebreak");
        // Three buckets, same version, no dependencies — should emit alpha, bravo, charlie
        for app in &["charlie", "bravo", "alpha"] {
            write_pending_json(
                &djogi::migrate::pending_json_path(
                    &work,
                    &BucketKey {
                        database: "main".to_string(),
                        app: app.to_string(),
                    },
                ),
                "main",
                app,
                "V20260609000000__initial",
                &[],
            );
        }

        let plans = discover_pending_plans(&work).expect("discovers");
        let apps: Vec<&str> = plans.iter().map(|p| p.bucket.app.as_str()).collect();
        assert_eq!(apps, ["alpha", "bravo", "charlie"]);
        let _ = fs::remove_dir_all(&work);
    }

    /// depends_on referencing a bucket NOT in the current pending set is
    /// silently ignored (REQ-398-6: already applied earlier / no delta this run).
    #[test]
    fn discover_depends_on_missing_bucket_is_ignored() {
        let work = temp_workspace("discover_pending_deps_missing");
        // system depends on billing, but billing has no pending file
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: "system".to_string(),
                },
            ),
            "main",
            "system",
            "V20260609000000__initial",
            &["billing"],
        );

        let plans = discover_pending_plans(&work).expect("discovers");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].bucket.app, "system");
        let _ = fs::remove_dir_all(&work);
    }

    /// Same-version buckets with a dependency cycle are refused at apply time
    /// (REQ-398-7 defensive half — compose should have caught this, but apply
    /// guards against hand-edited or corrupted pending files).
    #[test]
    fn discover_depends_on_cycle_is_refused() {
        let work = temp_workspace("discover_pending_deps_cycle");
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: "alpha".to_string(),
                },
            ),
            "main",
            "alpha",
            "V20260609000000__initial",
            &["beta"],
        );
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: "beta".to_string(),
                },
            ),
            "main",
            "beta",
            "V20260609000000__initial",
            &["alpha"],
        );

        let err = discover_pending_plans(&work).expect_err("cycle must be refused");
        assert!(
            err.contains("alpha") && err.contains("beta") && err.contains("cycle"),
            "error should name both apps and mention cycle, got: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// A singleton pending group whose `depends_on` carries a label that
    /// fails `is_acceptable_pending_path_component` must be refused. The
    /// singleton fast-path bypasses the topo-sort, so without the
    /// pre-fast-path validation loop a hand-edited or corrupted label
    /// (path traversal, embedded whitespace) would slip through silently.
    /// Drives `order_pending_groups_by_dependencies` directly because the
    /// invalid label lives inside the pending JSON, not in the filename
    /// that discovery already validates.
    #[test]
    fn single_bucket_with_invalid_depends_on_is_refused() {
        let make_singleton = |dep: &str| -> Vec<DiscoveredPendingPlan> {
            let plan = PendingPlan {
                format_version: djogi::migrate::PENDING_FORMAT_VERSION.to_string(),
                bucket_database: "main".to_string(),
                bucket_app: "system".to_string(),
                version: "V20260609000000__initial".to_string(),
                slug: "test".to_string(),
                model_snapshot: djogi::migrate::AppliedSchema {
                    djogi_version: "0.1.0".to_string(),
                    enums: std::collections::BTreeMap::new(),
                    format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                    generated_at: "2026-06-09T00:00:00Z".to_string(),
                    indexes: Vec::new(),
                    models: std::collections::BTreeMap::new(),
                    registered_apps: vec!["system".to_string()],
                },
                checksum_up: "V1:".to_string() + &"a".repeat(64),
                checksum_down: None,
                composed_at: "2026-06-09T00:00:00Z".to_string(),
                depends_on: vec![dep.to_string()],
            };
            vec![DiscoveredPendingPlan {
                path: PathBuf::from("target/djogi_pending/main/system.json"),
                bucket: BucketKey {
                    database: "main".to_string(),
                    app: "system".to_string(),
                },
                plan,
                is_phase_zero: false,
            }]
        };

        for bad_label in ["../traversal", "has space"] {
            let err = order_pending_groups_by_dependencies(make_singleton(bad_label))
                .expect_err("invalid singleton depends_on label must be refused");
            assert!(
                err.contains("invalid depends_on label")
                    && err.contains("main")
                    && err.contains("system"),
                "[{bad_label}] error must name database, app, and the invalid label: {err}"
            );
        }
    }

    /// End-to-end test: two buckets, same version, `system.event_log`
    /// FK→`users.users`, composed and applied through real Postgres.
    /// Asserts both tables exist, the FK constraint exists in pg_constraint,
    /// and the ledger has exactly two rows for the composed version.
    /// Uses `#[djogi_test]` for per-test database isolation (the macro drops
    /// the test database on normal return or caught panic).
    #[djogi::djogi_test]
    async fn cross_bucket_fk_applies_in_dependency_order(mut ctx: djogi::context::DjogiContext) {
        // Unique suffix for table names — avoids collisions when tests run in parallel.
        static E2E_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = E2E_COUNTER.fetch_add(1, Ordering::SeqCst);
        let users_table = format!("e2e_users_{n}");
        let event_log_table = format!("e2e_event_log_{n}");

        let work = temp_workspace("cross-bucket-fk-e2e");
        let guard = djogi::migrate::acquire_workspace_lock(
            &work.join(LOCK_FILE_NAME),
            std::time::Duration::from_secs(5),
        )
        .expect("lock workspace");

        // Construct models: users bucket (PK only) + system bucket (FK→users).
        let mut models: std::collections::BTreeMap<
            djogi::migrate::BucketKey,
            djogi::migrate::AppliedSchema,
        > = std::collections::BTreeMap::new();

        let users_bucket = BucketKey {
            database: "main".into(),
            app: "users".into(),
        };
        let system_bucket = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };

        {
            let mut users_schema = djogi::migrate::AppliedSchema {
                djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                enums: std::collections::BTreeMap::new(),
                format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-06-10T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models: std::collections::BTreeMap::new(),
                registered_apps: vec!["users".to_string()],
            };
            users_schema.models.insert(
                users_table.clone(),
                djogi::migrate::TableSchema {
                    app: Some("users".to_string()),
                    columns: vec![djogi::migrate::ColumnSchema {
                        name: "id".to_string(),
                        sql_type: "BIGINT".to_string(),
                        nullable: false,
                        default_sql: Some("heerid_next_desc()".to_string()),
                        ..default_col()
                    }],
                    primary_key: djogi::migrate::PrimaryKeySchema {
                        columns: vec!["id".to_string()],
                        kind: djogi::migrate::PkKindSchema::HeerIdRecencyBiased,
                    },
                    table: users_table.clone(),
                    ..default_table()
                },
            );
            models.insert(users_bucket.clone(), users_schema);
        }

        {
            let mut system_schema = djogi::migrate::AppliedSchema {
                djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                enums: std::collections::BTreeMap::new(),
                format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-06-10T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models: std::collections::BTreeMap::new(),
                registered_apps: vec!["system".to_string()],
            };
            system_schema.models.insert(
                event_log_table.clone(),
                djogi::migrate::TableSchema {
                    app: Some("system".to_string()),
                    columns: vec![
                        djogi::migrate::ColumnSchema {
                            name: "id".to_string(),
                            sql_type: "BIGINT".to_string(),
                            nullable: false,
                            default_sql: Some("heerid_next_desc()".to_string()),
                            ..default_col()
                        },
                        djogi::migrate::ColumnSchema {
                            name: "user_id".to_string(),
                            sql_type: "BIGINT".to_string(),
                            nullable: false,
                            foreign_key: Some(djogi::migrate::ForeignKeySchema {
                                deferrable: false,
                                initially_deferred: false,
                                on_delete: djogi::migrate::OnDeleteSchema::Restrict,
                                ref_column: "id".to_string(),
                                ref_table: users_table.clone(),
                            }),
                            ..default_col()
                        },
                    ],
                    primary_key: djogi::migrate::PrimaryKeySchema {
                        columns: vec!["id".to_string()],
                        kind: djogi::migrate::PkKindSchema::HeerIdRecencyBiased,
                    },
                    table: event_log_table.clone(),
                    ..default_table()
                },
            );
            models.insert(system_bucket.clone(), system_schema);
        }

        // Empty snapshots — fresh compose so differ sees all tables as new.
        let mut snapshots = std::collections::BTreeMap::new();
        for bucket in [&users_bucket, &system_bucket] {
            snapshots.insert(
                bucket.clone(),
                djogi::migrate::AppliedSchema {
                    djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                    enums: std::collections::BTreeMap::new(),
                    format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                    generated_at: "2026-06-10T00:00:00Z".to_string(),
                    indexes: Vec::new(),
                    models: std::collections::BTreeMap::new(),
                    registered_apps: vec![bucket.app.clone()],
                },
            );
        }

        let apps = vec![
            djogi::migrate::AppLifecycle {
                label: "users".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            djogi::migrate::AppLifecycle {
                label: "system".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        // Compose — generates pending files + migration SQL.
        let compose_req = djogi::migrate::ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "cross-bucket-fk",
            allow_destructive: false,
            force_overwrite: false,
            now: time::OffsetDateTime::UNIX_EPOCH
                + time::Duration::days(19726)
                + time::Duration::seconds(0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let compose_report = djogi::migrate::compose(compose_req).expect("compose");
        assert!(
            !compose_report.composed_buckets.is_empty(),
            "compose should produce delta buckets"
        );

        // Release the workspace lock before driving run_apply: run_apply acquires the
        // same lock internally (step 5). The lock was only needed for the compose
        // phase; holding it through the spawn_blocking call causes flock(LOCK_EX|LOCK_NB)
        // to return EWOULDBLOCK on the second open-file-description, blocking run_apply
        // for the full GUARD_DEFAULT_TIMEOUT (30 s) and returning exit code 1.
        drop(guard);

        // Extract the composed version from the report.
        let composed_version = &compose_report.composed_buckets[0].version;

        // `run_apply` reads config from a Djogi.toml file rather than accepting
        // a DjogiContext, so we construct the per-test database URL by querying
        // current_database() and replacing it in the admin DATABASE_URL.
        let test_db = ctx
            .raw_scalar::<String>("SELECT current_database()", &[])
            .await
            .expect("current_database");
        let admin_url = std::env::var("DATABASE_URL").expect(
            "DATABASE_URL must be set for djogi_test \
             (e.g. postgres://djogi:djogi@localhost:5432/djogi_test)",
        );
        let test_db_url = replace_db_in_url(&admin_url, &test_db)
            .expect("construct per-test database URL from DATABASE_URL");

        // Write minimal workspace config for run_apply.
        fs::write(
            work.join("Djogi.toml"),
            format!(
                "[database]\nurl = \"{test_db_url}\"\n\
                 max_connections = 1\ndev_mode = false\n\
                 [server]\nhost = \"127.0.0.1\"\nport = 8080\n"
            ),
        )
        .unwrap();

        // Drive the apply loop through run_apply (same path as `djogi migrations apply`).
        // spawn_blocking avoids a nested-runtime panic: djogi_test already owns a
        // tokio runtime; creating another with block_on from inside async context
        // panics. A blocking thread has no runtime, so the new runtime is safe there.
        let exit = {
            let work = work.clone();
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime")
                    .block_on(run_apply(
                        &work,
                        &FakeMode::Real,
                        None,
                        true, // single_node_dev: bypass node identity for E2E test
                    ))
            })
            .await
            .expect("spawn_blocking join")
        };

        assert_eq!(
            exit, 0,
            "apply should succeed (tables created in FK dependency order)"
        );

        // Assert 1: FK constraint exists on event_log → users.
        let fk_rows = ctx
            .raw_rows(
                "SELECT conname FROM pg_constraint \
                 WHERE conrelid = $1::regclass AND contype = 'f' AND confrelid = $2::regclass",
                &[&event_log_table.as_str(), &users_table.as_str()],
            )
            .await
            .expect("query pg_constraint");
        assert!(
            !fk_rows.is_empty(),
            "FK constraint should exist from {event_log_table} → {users_table}"
        );

        // Assert 2: Ledger has exactly TWO rows for the composed version
        // (one per bucket: users and system). Do NOT assert total row count —
        // phase-zero row also exists at PHASE_ZERO_VERSION.
        let ledger_rows = ctx
            .raw_rows(
                "SELECT app_label FROM djogi_schema_migrations \
                 WHERE version = $1 AND status = 'applied'",
                &[&composed_version.as_str()],
            )
            .await
            .expect("query ledger");
        assert_eq!(
            ledger_rows.len(),
            2,
            "ledger should have exactly 2 rows for composed version {composed_version} \
             (users + system), got {} rows",
            ledger_rows.len()
        );
        let app_labels: Vec<String> = ledger_rows
            .iter()
            .map(|row| row.try_get(0).expect("decode app_label"))
            .collect();
        assert!(
            app_labels.contains(&"users".to_string()),
            "ledger should have 'users' bucket: {app_labels:?}"
        );
        assert!(
            app_labels.contains(&"system".to_string()),
            "ledger should have 'system' bucket: {app_labels:?}"
        );

        // Assert 3: Verify ordering — users applied before system.
        let ordered_rows = ctx
            .raw_rows(
                "SELECT app_label, id FROM djogi_schema_migrations \
                 WHERE version = $1 AND status = 'applied' ORDER BY id",
                &[&composed_version.as_str()],
            )
            .await
            .expect("query ledger ordered");
        assert_eq!(ordered_rows[0].try_get::<_, String>(0).unwrap(), "users");
        assert_eq!(ordered_rows[1].try_get::<_, String>(0).unwrap(), "system");

        let _ = fs::remove_dir_all(&work);

        // Note: reverting the stage-2 topo sort in discover_pending_plans
        // (removing the order_pending_groups_by_dependencies call) would cause
        // this test to fail — `system` sorts before `users` alphabetically,
        // so the FK constraint on event_log.user_id → users.id would fire
        // before the users table exists (SQLSTATE 42P01 undefined_table).
    }

    /// Replace the database component of a Postgres URL with a new name.
    /// Mirrors `djogi::migrate::reset::replace_db_in_url`; inlined here
    /// so the test module does not depend on that internal path.
    fn replace_db_in_url(url: &str, new_db: &str) -> Option<String> {
        let body = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))?;
        let scheme = if url.starts_with("postgres://") {
            "postgres://"
        } else {
            "postgresql://"
        };
        let mut idx = 0usize;
        let body_bytes = body.as_bytes();
        while idx < body_bytes.len() && body_bytes[idx] != b'/' {
            idx += 1;
        }
        if idx >= body_bytes.len() {
            return None;
        }
        let authority = &body[..idx];
        let path_start = idx + 1;
        let mut path_end = path_start;
        while path_end < body_bytes.len() && body_bytes[path_end] != b'?' {
            path_end += 1;
        }
        let trailing = &body[path_end..];
        Some(format!("{scheme}{authority}/{new_db}{trailing}"))
    }

    fn default_col() -> djogi::migrate::ColumnSchema {
        djogi::migrate::ColumnSchema {
            check: None,
            codec: None,
            comment: None,
            default_sql: None,
            foreign_key: None,
            generated: None,
            identity: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: "".to_string(),
            nullable: false,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "".to_string(),
            unique: false,
            type_change_using: None,
        }
    }

    fn default_table() -> djogi::migrate::TableSchema {
        djogi::migrate::TableSchema {
            app: None,
            columns: Vec::new(),
            exclusion_constraints: Vec::new(),
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: Vec::new(),
                kind: djogi::migrate::PkKindSchema::Composite,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    #[test]
    fn discover_pending_plans_refuses_malformed_pending_json() {
        let work = temp_workspace("discover_pending_malformed");
        let path = djogi::migrate::pending_json_path(
            &work,
            &BucketKey {
                database: "main".to_string(),
                app: String::new(),
            },
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();

        let err = discover_pending_plans(&work).expect_err("malformed pending must refuse");
        assert!(err.contains("parse pending JSON"));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_pending_plans_refuses_hidden_phase_zero_database_mismatch() {
        let work = temp_workspace("discover_pending_phase_zero_db_mismatch");
        write_pending_json(
            &djogi::migrate::phase_zero_pending_json_path(
                &work,
                "main",
                djogi::migrate::PHASE_ZERO_VERSION,
            ),
            "other_db",
            "",
            djogi::migrate::PHASE_ZERO_VERSION,
            &[],
        );

        let err = discover_pending_plans(&work).expect_err("hidden Phase 0 mismatch must refuse");
        assert!(
            err.contains("expected main from path"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_pending_plans_refuses_normal_global_phase_zero_pending() {
        let work = temp_workspace("discover_pending_normal_global_phase_zero");
        let path = djogi::migrate::pending_json_path(
            &work,
            &BucketKey {
                database: "main".to_string(),
                app: String::new(),
            },
        );
        write_pending_json(&path, "main", "", djogi::migrate::PHASE_ZERO_VERSION, &[]);

        let err = discover_pending_plans(&work).expect_err("normal-global Phase 0 must refuse");
        assert!(
            err.contains("Phase 0") && err.contains(".phase_zero"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_pending_plans_refuses_normal_pending_app_mismatch() {
        let work = temp_workspace("discover_pending_normal_app_mismatch");
        let path = djogi::migrate::pending_json_path(
            &work,
            &BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
        );
        write_pending_json(&path, "main", "audit", "V20260606010101__mismatch", &[]);

        let err = discover_pending_plans(&work).expect_err("normal app mismatch must refuse");
        assert!(
            err.contains("expected billing from path"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn discover_pending_plans_refuses_noncanonical_normal_pending_filename() {
        let work = temp_workspace("discover_pending_noncanonical_filename");
        let path = work.join("target/djogi_pending/main/bad-name.json");
        write_pending_json(&path, "main", "bad-name", "V20260606010101__bad_name", &[]);

        let err = discover_pending_plans(&work).expect_err("non-canonical filename must refuse");
        assert!(
            err.contains("non-canonical app filename"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn load_verified_pending_for_apply_refuses_changed_artifact() {
        let work = temp_workspace("apply_pending_changed_after_discovery");
        let path = djogi::migrate::pending_json_path(
            &work,
            &BucketKey {
                database: "main".to_string(),
                app: String::new(),
            },
        );
        write_pending_json(&path, "main", "", "V20260606010101__stable", &[]);
        let discovered = discover_pending_plans(&work).expect("discover");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&PendingPlan {
                version: "V20260606010102__changed".to_string(),
                ..discovered[0].plan.clone()
            })
            .unwrap(),
        )
        .unwrap();

        let err = load_verified_pending_for_apply(&discovered[0])
            .expect_err("apply must refuse a changed pending artifact");
        assert!(
            err.contains("changed after discovery"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn reconcile_pending_plans_after_lock_refuses_added_artifact() {
        let work = temp_workspace("apply_pending_added_before_lock");
        let path = djogi::migrate::pending_json_path(
            &work,
            &BucketKey {
                database: "main".to_string(),
                app: String::new(),
            },
        );
        write_pending_json(&path, "main", "", "V20260606010101__stable", &[]);
        let discovered = discover_pending_plans(&work).expect("discover");
        write_pending_json(
            &djogi::migrate::phase_zero_pending_json_path(
                &work,
                "main",
                djogi::migrate::PHASE_ZERO_VERSION,
            ),
            "main",
            "",
            djogi::migrate::PHASE_ZERO_VERSION,
            &[],
        );

        let err = reconcile_pending_plans_after_lock(&work, &discovered)
            .expect_err("locked reconciliation must refuse a changed pending set");
        assert!(
            err.contains("changed while waiting for the workspace lock"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn reconcile_pending_plans_after_lock_accepts_unchanged_set() {
        let work = temp_workspace("apply_pending_stable_under_lock");
        let path = djogi::migrate::pending_json_path(
            &work,
            &BucketKey {
                database: "main".to_string(),
                app: String::new(),
            },
        );
        write_pending_json(&path, "main", "", "V20260606010101__stable", &[]);
        let discovered = discover_pending_plans(&work).expect("discover");

        let locked = reconcile_pending_plans_after_lock(&work, &discovered)
            .expect("unchanged set must reconcile");
        assert_eq!(locked, discovered);
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn repair_checksum_drift_is_identity_free() {
        let work = temp_workspace("repair_checksum_identity_free");
        write_unreachable_config(&work);

        let exit = without_database_url(|| {
            repair_checksum_drift_cmd(
                "V20260601000000__repair_checksum",
                None,
                None,
                Some("V1:0000000000000000000000000000000000000000000000000000000000000000"),
                None,
                Some(work.clone()),
            )
        });

        assert_eq!(
            exit,
            ExitCode::from(1),
            "checksum-drift should reach pool connection without shared identity validation"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn repair_partial_apply_is_identity_free() {
        let work = temp_workspace("repair_partial_identity_free");
        write_unreachable_config(&work);

        let exit = without_database_url(|| {
            repair_partial_apply_cmd(
                "V20260601000000__repair_partial",
                PartialApplyResolution::MarkRolledBack,
                "operator confirmed rollback",
                None,
                None,
                Some(work.clone()),
            )
        });

        assert_eq!(
            exit,
            ExitCode::from(1),
            "partial-apply should reach pool connection without shared identity validation"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn repair_snapshot_rebuild_is_identity_free() {
        let work = temp_workspace("repair_snapshot_identity_free");
        write_unreachable_config(&work);

        let exit = without_database_url(|| {
            repair_snapshot_rebuild_cmd(None, None, None, Some(work.clone()))
        });

        assert_eq!(
            exit,
            ExitCode::from(1),
            "snapshot-rebuild should reach pool connection without shared identity validation"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// `compose_with_inputs` must consume the disk-discovered buckets, not
    /// just the inventory's. We set up a
    /// `migrations/main/billing/schema_snapshot.json` with a `widgets`
    /// table, pass an EMPTY models map (simulating "billing app was
    /// removed from the workspace"), set `allow_destructive = true`,
    /// and assert the resulting up SQL contains `DROP TABLE
    /// "widgets"`. If the disk-walk regressed and `compose_with_inputs`
    /// only loaded snapshots for inventory-known buckets, the differ
    /// would never see billing's snapshot and the compose would exit
    /// `NothingToCompose` (no DROP, no SQL written).
    /// End-to-end regression guard.
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
                    codec: None,
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
            &[AppLifecycle {
                label: "billing".to_string(),
                database: "main".to_string(),
                renamed_from: None,
                tombstone: true, // intentional removal channel
            }],
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

    /// A cross-bucket foreign-key cycle surfaced by `compose` must map to
    /// exit code 2 through `compose_with_inputs` — an operator-actionable
    /// refusal, not the exit-1 unexpected-error catch-all. The cycle is
    /// injected at the model level: app `a`'s table references app `b`'s
    /// table and vice versa, so no slice apply order satisfies both FKs
    /// and `compose` returns
    /// [`ComposeError::CrossBucketForeignKeyCycle`]. Before the dedicated
    /// arm was added this fell through to the catch-all and exited 1.
    #[test]
    fn compose_cycle_exits_with_code_two() {
        use djogi::migrate::projection::BucketKey;
        use djogi::migrate::schema::{
            AppliedSchema, ColumnSchema, ForeignKeySchema, OnDeleteSchema, PkKindSchema,
            PrimaryKeySchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
        };
        use std::collections::BTreeMap;

        let work = temp_workspace("compose_cycle_exit_two");

        // A column that foreign-keys to `target_table.id`.
        let fk_col = |name: &str, target_table: &str| -> ColumnSchema {
            ColumnSchema {
                name: name.to_string(),
                sql_type: "BIGINT".to_string(),
                foreign_key: Some(ForeignKeySchema {
                    deferrable: false,
                    initially_deferred: false,
                    on_delete: OnDeleteSchema::Restrict,
                    ref_column: "id".to_string(),
                    ref_table: target_table.to_string(),
                }),
                ..default_col()
            }
        };

        // A table with a HeerId PK `id` column and one FK column.
        let table_with_fk =
            |app: &str, table: &str, fk_name: &str, fk_target: &str| -> TableSchema {
                let id_col = ColumnSchema {
                    name: "id".to_string(),
                    sql_type: "BIGINT".to_string(),
                    default_sql: Some("heerid_next_desc()".to_string()),
                    ..default_col()
                };
                TableSchema {
                    app: Some(app.to_string()),
                    columns: vec![id_col, fk_col(fk_name, fk_target)],
                    primary_key: PrimaryKeySchema {
                        columns: vec!["id".to_string()],
                        kind: PkKindSchema::HeerIdRecencyBiased,
                    },
                    table: table.to_string(),
                    ..default_table()
                }
            };

        let schema_for =
            |app: &str, table: &str, fk_name: &str, fk_target: &str| -> AppliedSchema {
                let mut models = BTreeMap::new();
                models.insert(
                    table.to_string(),
                    table_with_fk(app, table, fk_name, fk_target),
                );
                AppliedSchema {
                    djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                    enums: BTreeMap::new(),
                    format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
                    generated_at: "2026-06-10T00:00:00Z".to_string(),
                    indexes: Vec::new(),
                    models,
                    registered_apps: vec![app.to_string()],
                }
            };

        let a_bucket = BucketKey {
            database: "main".into(),
            app: "a".into(),
        };
        let b_bucket = BucketKey {
            database: "main".into(),
            app: "b".into(),
        };

        // a.table_a.b_id → b.table_b ; b.table_b.a_id → a.table_a (cycle).
        let mut models: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
        models.insert(a_bucket, schema_for("a", "table_a", "b_id", "table_b"));
        models.insert(b_bucket, schema_for("b", "table_b", "a_id", "table_a"));

        let now = time::OffsetDateTime::from_unix_timestamp(1_749_513_600).unwrap();
        let exit = compose_with_inputs(
            &work,
            "cross-bucket cycle",
            false, // allow_destructive — irrelevant; the cycle refuses first
            false, // force_overwrite
            &models,
            &[
                AppLifecycle {
                    label: "a".to_string(),
                    database: "main".to_string(),
                    renamed_from: None,
                    tombstone: false,
                },
                AppLifecycle {
                    label: "b".to_string(),
                    database: "main".to_string(),
                    renamed_from: None,
                    tombstone: false,
                },
            ],
            now,
            None, // pk_flip_join_table_option — no flip in this test
        );

        assert_eq!(
            exit,
            ExitCode::from(2),
            "a cross-bucket FK cycle must exit 2 (operator-actionable refusal), not 1"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// `status_cmd` invokes its tokio runtime and
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

    // ── AttuneError → exit code matrix ──────────────────────────────

    /// Every `AttuneError::Refused(_)` variant must map to exit code `2`
    /// per `docs/spec/configuration.md` §14. The pre-fix implementation
    /// flattened every error to `1`, so an operator running attune in CI
    /// could not distinguish "policy gate refused before any side effect"
    /// from "ran half a step and failed mid-flight".
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
            // Dev_mode and DJOGI_ENV gates both produce `AttuneError::Refused(_)`
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

    // ── issue #354: baseline exit-code mapping ──────────────────────────

    /// The refusal-class `RunnerError` variants the baseline path can
    /// `baseline_cmd` validates the `--reason` guard before any DB
    /// work. An empty or whitespace-only reason must return exit 2
    /// without touching the filesystem or network — the guard fires
    /// on the CLI-owned string before the tokio runtime is even built.
    #[test]
    fn baseline_empty_reason_exits_code_2() {
        let result = baseline_cmd(
            "V00000000000000__baseline",
            "description",
            "",
            None,
            None,
            Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws")),
            None,  // node_id
            false, // single_node_dev
        );
        assert_eq!(
            result,
            ExitCode::from(2),
            "empty --reason must exit 2 before any DB work"
        );
    }

    #[test]
    fn baseline_whitespace_reason_exits_code_2() {
        let result = baseline_cmd(
            "V00000000000000__baseline",
            "description",
            "   ",
            None,
            None,
            Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws")),
            None,  // node_id
            false, // single_node_dev
        );
        assert_eq!(
            result,
            ExitCode::from(2),
            "whitespace-only --reason must exit 2 before any DB work"
        );
    }

    /// surface must map to exit `2` — a blind retry would hit the same
    /// condition, so CI must treat them as "operator must intervene"
    /// rather than retryable. A duplicate baseline version (terminal or
    /// non-terminal), a wiring bug that supplies a snapshot, and an
    /// out-of-order rejection are all refusals.
    #[test]
    fn baseline_refusal_variants_map_to_exit_code_two() {
        let cases = [
            RunnerError::VersionAlreadyApplied {
                version: "V00000000000000__baseline".to_string(),
                applied_at: None,
            },
            RunnerError::VersionCollisionNonTerminal {
                version: "V00000000000000__baseline".to_string(),
                status: LedgerStatus::Pending,
                run_id: 1,
            },
            RunnerError::BaselineSnapshotShouldNotBeProvided,
            RunnerError::AdvisoryUnlockReturnedFalse {
                bucket: BucketKey {
                    database: "main".to_string(),
                    app: String::new(),
                },
                key: 0x0102_0304_0506_0708,
            },
            RunnerError::OutOfOrderRejected {
                version: "V00000000000000__baseline".to_string(),
                conflicting_version: "V20260101000000__later".to_string(),
                conflicting_applied_at: None,
            },
        ];
        for err in &cases {
            assert_eq!(
                baseline_error_exit_code(err),
                2,
                "baseline refusal variant must map to exit 2: {err}"
            );
        }
    }

    /// Transient `RunnerError` variants reachable from the baseline path
    /// must map to exit `1` (retryable). The `#[non_exhaustive]`
    /// wildcard arm guarantees any unnamed variant also lands on `1`;
    /// these representative cases pin the projection / ledger / snapshot
    /// failure shapes the baseline runner can actually emit.
    #[test]
    fn baseline_transient_variants_map_to_exit_code_one() {
        use djogi::error::{DbError, DjogiError};
        let cases = [
            RunnerError::LedgerBootstrapFailed {
                source: DjogiError::Db(DbError::other("create table failed")),
            },
            RunnerError::LedgerWriteFailed {
                version: "V00000000000000__baseline".to_string(),
                source: DjogiError::Db(DbError::other("insert failed")),
            },
            RunnerError::PinnedSessionCheckoutFailed {
                source: DjogiError::Db(DbError::other("pool exhausted")),
            },
            RunnerError::AdvisoryLockFailed {
                bucket: BucketKey {
                    database: "main".to_string(),
                    app: String::new(),
                },
                key: 0x0102_0304_0506_0708,
                attempts: 3,
            },
        ];
        for err in &cases {
            assert_eq!(
                baseline_error_exit_code(err),
                1,
                "baseline transient variant must map to exit 1: {err}"
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
            None,  // node_id
            false, // single_node_dev
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
            None,  // node_id
            false, // single_node_dev
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
            None,  // node_id
            false, // single_node_dev
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
            None, // node_id — identity resolution is tested separately;
            true, // single_node_dev — provide explicit dev mode to bypass resolver
        );
        // Should be 1 (config error) not 2 (refusal)
        assert_ne!(
            result,
            ExitCode::from(2),
            "--reason without --fake should not refuse"
        );
    }

    // ── render_verify_report ─────────────────────────────────────
    // `render_verify_report` returns `Vec<String>` so the rendering is
    // assertable without capturing stdout. Each test pins the exact lines
    // the operator sees for one report shape.

    /// Build a bucket for render tests.
    fn render_bucket(database: &str, app: &str) -> djogi::migrate::BucketKey {
        djogi::migrate::BucketKey {
            database: database.to_string(),
            app: app.to_string(),
        }
    }

    /// Construct a [`VerifyDiagnostic`] tersely for render tests.
    fn diag(
        code: &str,
        severity: djogi::migrate::VerifySeverity,
        message: &str,
        location: Option<&str>,
    ) -> djogi::migrate::VerifyDiagnostic {
        djogi::migrate::VerifyDiagnostic {
            code: code.to_string(),
            severity,
            message: message.to_string(),
            location: location.map(str::to_string),
        }
    }

    #[test]
    fn render_verify_report_clean_output() {
        use djogi::migrate::VerifyReport;

        let report = VerifyReport {
            diagnostics: vec![],
            latest_applied_version: Some("001_initial".to_string()),
            applied_count: 3,
            unfinished_count: 0,
        };
        let bucket = render_bucket("main", "");

        let lines = render_verify_report(&report, &bucket);

        assert!(
            lines.contains(&"Ledger: 3 applied, latest 001_initial".to_string()),
            "missing ledger line; got {lines:?}"
        );
        assert!(
            lines.contains(&"No drift detected. Schema is consistent.".to_string()),
            "missing clean line; got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Result: PASSED")),
            "missing PASSED result; got {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("FAILED")),
            "clean report must not say FAILED; got {lines:?}"
        );
    }

    #[test]
    fn render_verify_report_with_errors() {
        use djogi::migrate::{VerifyReport, VerifySeverity};

        // Diagnostics are pre-sorted by `(code, location)` exactly as the
        // library returns them — render does not re-sort.
        let report = VerifyReport {
            diagnostics: vec![
                diag(
                    "D601",
                    VerifySeverity::Error,
                    "Snapshot table missing from live DB",
                    Some("users"),
                ),
                diag(
                    "D611",
                    VerifySeverity::Warning,
                    "Live index not present in snapshot",
                    Some("idx_posts_created"),
                ),
            ],
            latest_applied_version: Some("V20260501000000__add_users".to_string()),
            applied_count: 2,
            unfinished_count: 0,
        };
        let bucket = render_bucket("main", "myapp");

        assert!(report.has_errors());
        let lines = render_verify_report(&report, &bucket);

        assert!(
            lines
                .contains(&"[ERROR] D601 (users): Snapshot table missing from live DB".to_string()),
            "missing D601 line; got {lines:?}"
        );
        assert!(
            lines.contains(
                &"[WARN] D611 (idx_posts_created): Live index not present in snapshot".to_string()
            ),
            "missing D611 line; got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Result: FAILED")),
            "error report must say FAILED; got {lines:?}"
        );
    }

    #[test]
    fn render_verify_report_header_shows_global_and_named_app() {
        use djogi::migrate::VerifyReport;

        let report = VerifyReport {
            diagnostics: vec![],
            latest_applied_version: None,
            applied_count: 0,
            unfinished_count: 0,
        };

        // Empty app label → `_global_` in the header.
        let global = render_verify_report(&report, &render_bucket("main", ""));
        assert_eq!(
            global.first().map(String::as_str),
            Some("djogi migrations verify — main/_global_"),
            "global bucket header; got {global:?}"
        );

        // Named app → the label verbatim in the header.
        let named = render_verify_report(&report, &render_bucket("crud_log", "billing"));
        assert_eq!(
            named.first().map(String::as_str),
            Some("djogi migrations verify — crud_log/billing"),
            "named bucket header; got {named:?}"
        );
    }

    #[test]
    fn render_verify_report_warning_only_passes_with_warnings() {
        use djogi::migrate::{VerifyReport, VerifySeverity};

        let report = VerifyReport {
            diagnostics: vec![diag(
                "D606",
                VerifySeverity::Warning,
                "type differs (advisory)",
                Some("users.age"),
            )],
            latest_applied_version: Some("001_initial".to_string()),
            applied_count: 1,
            unfinished_count: 0,
        };
        let lines = render_verify_report(&report, &render_bucket("main", ""));

        assert!(
            lines
                .iter()
                .any(|l| l.contains("Result: PASSED with warnings")),
            "warning-only must PASS with warnings; got {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("FAILED")),
            "warning-only must not say FAILED; got {lines:?}"
        );
    }

    #[test]
    fn render_verify_report_empty_ledger_line() {
        use djogi::migrate::VerifyReport;

        let report = VerifyReport {
            diagnostics: vec![],
            latest_applied_version: None,
            applied_count: 0,
            unfinished_count: 0,
        };
        let lines = render_verify_report(&report, &render_bucket("main", ""));

        assert!(
            lines.contains(&"Ledger: empty (no migrations applied yet)".to_string()),
            "empty ledger line; got {lines:?}"
        );
    }

    #[test]
    fn render_verify_report_unfinished_ledger_line() {
        use djogi::migrate::VerifyReport;

        let report = VerifyReport {
            diagnostics: vec![],
            latest_applied_version: Some("V20260501000000__add_users".to_string()),
            applied_count: 2,
            unfinished_count: 1,
        };
        let lines = render_verify_report(&report, &render_bucket("main", ""));

        assert!(
            lines.contains(
                &"Ledger: 2 applied, 1 unfinished, latest V20260501000000__add_users".to_string()
            ),
            "unfinished ledger line; got {lines:?}"
        );
    }

    #[test]
    fn render_verify_report_info_with_no_location_uses_dash() {
        use djogi::migrate::{VerifyReport, VerifySeverity};

        // An Info diagnostic with `location: None` exercises the
        // `unwrap_or("-")` path, and the all-info summary line.
        let report = VerifyReport {
            diagnostics: vec![diag(
                "D692",
                VerifySeverity::Info,
                "enum type(s) declared; not yet checked",
                None,
            )],
            latest_applied_version: Some("001_initial".to_string()),
            applied_count: 1,
            unfinished_count: 0,
        };
        let lines = render_verify_report(&report, &render_bucket("main", ""));

        assert!(
            lines.iter().any(|l| l.contains("(-)")),
            "location: None must render as (-); got {lines:?}"
        );
        assert!(
            lines.contains(&"Result: PASSED (1 info(s))".to_string()),
            "all-info summary; got {lines:?}"
        );
    }

    // ── resolve_bucket_url (Class A) ─────────────────────────────────────

    fn db_config(
        url: &str,
        crud_log_url: Option<&str>,
        event_log_url: Option<&str>,
    ) -> djogi::config::DatabaseConfig {
        djogi::config::DatabaseConfig {
            url: url.to_string(),
            crud_log_url: crud_log_url.map(str::to_string),
            event_log_url: event_log_url.map(str::to_string),
            max_connections: None,
            dev_mode: false,
        }
    }

    #[test]
    fn resolve_bucket_url_main_uses_app_url_verbatim() {
        // "main" must use the app URL verbatim even when the path
        // component is not literally "main" — deriving would target a
        // database that does not exist.
        let cfg = db_config("postgres://user:pass@localhost:5432/myapp_prod", None, None);
        assert_eq!(
            resolve_bucket_url(&cfg, "main").as_deref(),
            Some("postgres://user:pass@localhost:5432/myapp_prod"),
            "main must return the app URL unchanged"
        );
    }

    #[test]
    fn resolve_bucket_url_crud_log_prefers_explicit_url() {
        let cfg = db_config(
            "postgres://localhost/main",
            Some("postgres://localhost/explicit_crud"),
            None,
        );
        assert_eq!(
            resolve_bucket_url(&cfg, "crud_log").as_deref(),
            Some("postgres://localhost/explicit_crud"),
            "crud_log must prefer the explicit crud_log_url"
        );
    }

    #[test]
    fn resolve_bucket_url_event_log_prefers_explicit_url() {
        let cfg = db_config(
            "postgres://localhost/main",
            None,
            Some("postgres://localhost/explicit_event"),
        );
        assert_eq!(
            resolve_bucket_url(&cfg, "event_log").as_deref(),
            Some("postgres://localhost/explicit_event"),
            "event_log must prefer the explicit event_log_url"
        );
    }

    #[test]
    fn resolve_bucket_url_empty_explicit_log_url_falls_back_to_derived() {
        // An empty explicit URL is treated as absent — derive from the app
        // URL's path component instead.
        let cfg = db_config("postgres://localhost/main", Some(""), Some("   "));
        // crud_log: empty string → derive.
        assert_eq!(
            resolve_bucket_url(&cfg, "crud_log").as_deref(),
            Some("postgres://localhost/crud_log"),
            "empty crud_log_url must fall back to derived"
        );
        // event_log: whitespace is NOT empty, so it is used verbatim — the
        // emptiness check is a strict `is_empty`, matching the spec.
        assert_eq!(
            resolve_bucket_url(&cfg, "event_log").as_deref(),
            Some("   "),
            "non-empty (whitespace) event_log_url is used verbatim"
        );
    }

    #[test]
    fn resolve_bucket_url_other_database_derives_from_app_url() {
        let cfg = db_config("postgres://user:pass@localhost:5432/main", None, None);
        assert_eq!(
            resolve_bucket_url(&cfg, "analytics").as_deref(),
            Some("postgres://user:pass@localhost:5432/analytics"),
            "an arbitrary database name derives by path splice"
        );
    }

    #[test]
    fn resolve_bucket_url_pathless_url_returns_none() {
        // A URL with no recognisable path component cannot be derived.
        let cfg = db_config("postgres://localhost", None, None);
        assert_eq!(
            resolve_bucket_url(&cfg, "crud_log"),
            None,
            "pathless URL must yield None for a derived database"
        );
    }

    #[test]
    fn resolve_bucket_url_pathless_url_still_returns_main_verbatim() {
        // "main" short-circuits before derivation, so even a pathless URL
        // returns it verbatim — the app pool is the operator's to define.
        let cfg = db_config("postgres://localhost", None, None);
        assert_eq!(
            resolve_bucket_url(&cfg, "main").as_deref(),
            Some("postgres://localhost"),
            "main returns the app URL verbatim regardless of path"
        );
    }

    #[test]
    fn resolve_apply_target_urls_uses_pending_bucket_databases() {
        let work = temp_workspace("apply_target_urls");
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "main".to_string(),
                    app: String::new(),
                },
            ),
            "main",
            "",
            "V20260607010101__main_global",
            &[],
        );
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "crud_log".to_string(),
                    app: "audit".to_string(),
                },
            ),
            "crud_log",
            "audit",
            "V20260607010102__crud_log_audit",
            &[],
        );

        let discovered = discover_pending_plans(&work).expect("discover");
        let cfg = db_config(
            "postgres://user:pass@localhost:5432/myapp_prod",
            Some("postgres://user:pass@localhost:5432/myapp_crud"),
            None,
        );

        let urls = resolve_apply_target_urls(&discovered, &cfg).expect("resolve");
        assert_eq!(
            urls.len(),
            2,
            "apply must preserve distinct target databases"
        );
        assert_eq!(
            urls.get("main").map(String::as_str),
            Some("postgres://user:pass@localhost:5432/myapp_prod"),
            "main pending plans must keep the app database URL"
        );
        assert_eq!(
            urls.get("crud_log").map(String::as_str),
            Some("postgres://user:pass@localhost:5432/myapp_crud"),
            "crud_log pending plans must route through the crud_log database URL"
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn resolve_apply_target_urls_refuses_unresolvable_pending_database() {
        let work = temp_workspace("apply_target_urls_unresolvable");
        write_pending_json(
            &djogi::migrate::pending_json_path(
                &work,
                &BucketKey {
                    database: "analytics".to_string(),
                    app: String::new(),
                },
            ),
            "analytics",
            "",
            "V20260607010103__analytics_global",
            &[],
        );

        let discovered = discover_pending_plans(&work).expect("discover");
        let cfg = db_config("postgres://localhost", None, None);
        let err = resolve_apply_target_urls(&discovered, &cfg)
            .expect_err("pathless app URL must refuse a derived pending database");
        assert!(err.contains("analytics"), "unexpected error: {err}");
        let _ = fs::remove_dir_all(&work);
    }

    // ── Stage 4D: CLI cleanup identity-free Phase 0 guard ─────────────

    #[test]
    fn classify_phase_zero_bytes_identity_free_production_is_ok() {
        let sql = current_production_phase_zero_sql("current_bytes");
        assert!(
            classify_phase_zero_bytes(sql.as_bytes()).is_none(),
            "production Phase 0 should be identity-free replay-current (no refusal)"
        );
    }

    #[test]
    fn classify_phase_zero_bytes_seed_capable_is_refused() {
        let sql = seed_capable_phase_zero_sql();
        let refusal = classify_phase_zero_bytes(sql.as_bytes());
        assert!(
            refusal.is_some(),
            "seed-capable Phase 0 should be refused by cleanup guard"
        );
        assert!(refusal.unwrap().contains("seed-capable"));
    }

    #[test]
    fn classify_phase_zero_bytes_generated_stale_is_refused() {
        let sql = generated_stale_phase_zero_sql("stale_bytes");
        let refusal = classify_phase_zero_bytes(sql.as_bytes());
        assert!(
            refusal.is_some(),
            "generated-stale Phase 0 should be refused"
        );
        assert!(refusal.unwrap().contains("generated-stale"));
    }

    #[test]
    fn classify_phase_zero_bytes_markerless_seed_is_refused() {
        let sql = markerless_seed_phase_zero_sql("markerless_seed_bytes");
        let refusal = classify_phase_zero_bytes(sql.as_bytes());
        assert!(
            refusal.is_some(),
            "markerless seed Phase 0 should be refused by cleanup guard"
        );
        assert!(refusal.unwrap().contains("seed-dml"));
    }

    #[test]
    fn classify_phase_zero_bytes_extended_seed_dml_forms_are_refused() {
        for (name, statement) in extended_seed_statement_cases() {
            let sql =
                phase_zero_with_seed_statement(&format!("extended_seed_bytes_{name}"), statement);
            let refusal = classify_phase_zero_bytes(sql.as_bytes());
            let msg = refusal.expect("extended seed Phase 0 should be refused");
            assert!(msg.contains("seed-dml"), "refusal reason: {msg}");
        }
    }

    #[test]
    fn classify_phase_zero_bytes_ambiguous_is_refused() {
        // Hand-edited or ambiguous Phase 0.
        let sql = "CREATE SCHEMA IF NOT EXISTS heer;\n\
                   ALTER DATABASE \"mydb\" SET heer.node_id = '1';\n";
        let refusal = classify_phase_zero_bytes(sql.as_bytes());
        assert!(refusal.is_some(), "ambiguous Phase 0 should be refused");
        assert!(refusal.unwrap().contains("ambiguous"));
    }

    #[test]
    fn classify_phase_zero_bytes_missing_is_refused() {
        let refusal = classify_phase_zero_bytes(b"  \n\t  ");
        assert!(refusal.is_some(), "missing Phase 0 should be refused");
        assert!(refusal.unwrap().contains("missing"));
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_stale_replay_plan() {
        let work = temp_workspace("stale_cleanup");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        // Write a stale replay plan JSON.
        let replay = CliReplayPlan {
            format_version: CLI_REPLAY_PLAN_FORMAT_VERSION.to_string(),
            classification: CliClassification::Additive,
            checksum_up: "V1:aabbccdd".to_string(),
            checksum_down: None,
            segments: vec![CliReplaySegment {
                kind: CliSegmentKind::Transactional,
                statements: vec![CliReplayStatement {
                    label: "phase_zero_bootstrap".to_string(),
                    up: generated_stale_phase_zero_sql("stale_replay"),
                }],
            }],
        };
        fs::write(
            bucket_dir.join("V00000000000000__phase_zero_bootstrap.plan.json"),
            serde_json::to_string(&replay).unwrap(),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:aabbccdd",
            None,
        );
        assert!(
            refusal.is_some(),
            "stale Phase 0 replay plan should be refused by cleanup guard"
        );
        let msg = refusal.unwrap();
        assert!(msg.contains("generated-stale"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_allows_current_replay_plan() {
        let work = temp_workspace("current_cleanup");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        // Write a current (production) replay plan JSON.
        let replay = CliReplayPlan {
            format_version: CLI_REPLAY_PLAN_FORMAT_VERSION.to_string(),
            classification: CliClassification::Additive,
            checksum_up: "V1:eeff0011".to_string(),
            checksum_down: None,
            segments: vec![CliReplaySegment {
                kind: CliSegmentKind::Transactional,
                statements: vec![CliReplayStatement {
                    label: "phase_zero_bootstrap".to_string(),
                    up: current_production_phase_zero_sql("current_replay"),
                }],
            }],
        };
        fs::write(
            bucket_dir.join("V00000000000000__phase_zero_bootstrap.plan.json"),
            serde_json::to_string(&replay).unwrap(),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:eeff0011",
            None,
        );
        assert!(
            refusal.is_none(),
            "identity-free Phase 0 should be allowed by cleanup guard; got: {refusal:?}"
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_seed_capable_replay_plan() {
        let work = temp_workspace("seed_cleanup_replay_plan");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let replay = CliReplayPlan {
            format_version: CLI_REPLAY_PLAN_FORMAT_VERSION.to_string(),
            classification: CliClassification::Additive,
            checksum_up: "V1:11223344".to_string(),
            checksum_down: None,
            segments: vec![CliReplaySegment {
                kind: CliSegmentKind::Transactional,
                statements: vec![CliReplayStatement {
                    label: "phase_zero_bootstrap".to_string(),
                    up: seed_capable_phase_zero_sql(),
                }],
            }],
        };
        fs::write(
            bucket_dir.join("V00000000000000__phase_zero_bootstrap.plan.json"),
            serde_json::to_string(&replay).unwrap(),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:11223344",
            None,
        );
        let msg = refusal.expect("seed-capable replay plan must refuse");
        assert!(msg.contains("seed-capable"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_markerless_seed_replay_plan() {
        let work = temp_workspace("markerless_seed_cleanup_replay_plan");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let replay = CliReplayPlan {
            format_version: CLI_REPLAY_PLAN_FORMAT_VERSION.to_string(),
            classification: CliClassification::Additive,
            checksum_up: "V1:55667788".to_string(),
            checksum_down: None,
            segments: vec![CliReplaySegment {
                kind: CliSegmentKind::Transactional,
                statements: vec![CliReplayStatement {
                    label: "phase_zero_bootstrap".to_string(),
                    up: markerless_seed_phase_zero_sql("markerless_seed_replay"),
                }],
            }],
        };
        fs::write(
            bucket_dir.join("V00000000000000__phase_zero_bootstrap.plan.json"),
            serde_json::to_string(&replay).unwrap(),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:55667788",
            None,
        );
        let msg = refusal.expect("markerless seed replay plan must refuse");
        assert!(msg.contains("seed-dml"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_cte_seed_dml_replay_plan() {
        let work = temp_workspace("cte_seed_cleanup_replay_plan");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let replay = CliReplayPlan {
            format_version: CLI_REPLAY_PLAN_FORMAT_VERSION.to_string(),
            classification: CliClassification::Additive,
            checksum_up: "V1:66778899".to_string(),
            checksum_down: None,
            segments: vec![CliReplaySegment {
                kind: CliSegmentKind::Transactional,
                statements: vec![CliReplayStatement {
                    label: "phase_zero_bootstrap".to_string(),
                    up: phase_zero_with_seed_statement(
                        "cte_seed_cleanup_replay",
                        "WITH rows AS (SELECT 1) INSERT INTO heer.heer_nodes (id) VALUES (1);",
                    ),
                }],
            }],
        };
        fs::write(
            bucket_dir.join("V00000000000000__phase_zero_bootstrap.plan.json"),
            serde_json::to_string(&replay).unwrap(),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:66778899",
            None,
        );
        let msg = refusal.expect("CTE seed replay plan must refuse");
        assert!(msg.contains("seed-dml"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_fallback_sql_file() {
        let work = temp_workspace("fallback_cleanup");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let up_sql = current_production_phase_zero_sql("fallback_sql");
        let up_filename = djogi::migrate::up_filename(djogi::migrate::PHASE_ZERO_VERSION);
        fs::write(bucket_dir.join(&up_filename), up_sql).unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:anychecksum",
            None,
        );
        assert!(
            refusal.is_none(),
            "identity-free Phase 0 fallback SQL should be allowed; got: {refusal:?}"
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_seed_capable_fallback_sql_file() {
        let work = temp_workspace("seed_cleanup_fallback");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let up_filename = djogi::migrate::up_filename(djogi::migrate::PHASE_ZERO_VERSION);
        fs::write(bucket_dir.join(&up_filename), seed_capable_phase_zero_sql()).unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:anychecksum",
            None,
        );
        let msg = refusal.expect("seed-capable fallback SQL must refuse");
        assert!(msg.contains("seed-capable"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_markerless_seed_fallback_sql_file() {
        let work = temp_workspace("markerless_seed_cleanup_fallback");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let up_filename = djogi::migrate::up_filename(djogi::migrate::PHASE_ZERO_VERSION);
        fs::write(
            bucket_dir.join(&up_filename),
            markerless_seed_phase_zero_sql("markerless_seed_fallback"),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:anychecksum",
            None,
        );
        let msg = refusal.expect("markerless seed fallback SQL must refuse");
        assert!(msg.contains("seed-dml"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn classify_phase_zero_for_cleanup_refuses_copy_from_seed_fallback_sql_file() {
        let work = temp_workspace("copy_seed_cleanup_fallback");
        let bucket_dir = work.join("migrations/main/_global_");
        fs::create_dir_all(&bucket_dir).unwrap();

        let up_filename = djogi::migrate::up_filename(djogi::migrate::PHASE_ZERO_VERSION);
        fs::write(
            bucket_dir.join(&up_filename),
            phase_zero_with_seed_statement(
                "copy_seed_cleanup_fallback",
                "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM STDIN;",
            ),
        )
        .unwrap();

        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let refusal = classify_phase_zero_for_cleanup(
            &work,
            &bucket,
            djogi::migrate::PHASE_ZERO_VERSION,
            "V1:anychecksum",
            None,
        );
        let msg = refusal.expect("COPY FROM seed fallback SQL must refuse");
        assert!(msg.contains("seed-dml"), "refusal reason: {msg}");

        let _ = fs::remove_dir_all(&work);
    }

    // ── per-app version-stream test ─────────────────────────────────

    #[djogi::djogi_test]
    async fn check_ledger_state_is_app_scoped(mut ctx: djogi::context::DjogiContext) {
        use djogi::migrate::{ExecutionMode, LedgerRow, LedgerStatus};

        // Bootstrap the ledger table so insert_pending works.
        djogi::migrate::bootstrap_ledger(&mut ctx)
            .await
            .expect("bootstrap");

        // Seed one applied row for app "users" at version V.
        let row = LedgerRow {
            version: "V20260609000000__t397".into(),
            description: "test migration".into(),
            checksum_up: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            checksum_down: Some(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            ),
            execution_mode: ExecutionMode::Transactional,
            status: LedgerStatus::Pending,
            execution_time_ms: 0,
            out_of_order_flag: false,
            applied_steps_count: 0,
            total_steps: None,
            partial_apply_note: None,
            run_id: 1,
            snapshot_version: "0".into(),
            app_label: "users".into(),
            leaf_identity: None,
        };
        let ledger_id = djogi::migrate::insert_pending_ledger_row(&mut ctx, &row)
            .await
            .expect("insert pending");
        djogi::migrate::mark_ledger_applied(&mut ctx, ledger_id, 10, 1)
            .await
            .expect("mark applied");

        // Different app stream must be NotPresent.
        let state = check_ledger_state(&mut ctx, "V20260609000000__t397", "system").await;
        assert!(
            matches!(state, LedgerState::NotPresent),
            "different app stream must be NotPresent, got {state:?}",
        );

        // Same app stream must be AlreadyApplied.
        let state = check_ledger_state(&mut ctx, "V20260609000000__t397", "users").await;
        assert!(
            matches!(state, LedgerState::AlreadyApplied),
            "same app stream must be AlreadyApplied, got {state:?}",
        );
    }
}
