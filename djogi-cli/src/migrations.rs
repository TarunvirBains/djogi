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
    AppLifecycle, AttuneError, AttuneMode, AttuneRequest, AutoEmitError, BootstrapError, BucketKey,
    ComposeError, ComposeRequest, DescriptorProvider, DiffError, DriftBaseline,
    GUARD_DEFAULT_TIMEOUT, LOCK_FILE_NAME, PartialApplyResolution, PendingPlan, RepairConfirmation,
    RepairError, RepairReport, RollbackError, RunnerCtx, RunnerError, SnapshotError, SqlEmitError,
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
/// Falls back to reading up/down SQL files and reconstructing the
/// canonical fallback plan when the replay plan JSON is absent.
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

    let sidecar_bytes = match std::fs::read(&replay_plan_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(ApplyReplayPlanError::PlanRead {
                path: replay_plan_path.clone(),
                source: e.to_string(),
            });
        }
    };

    if let Some(bytes) = sidecar_bytes {
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

    // Fallback: read SQL files and reconstruct the canonical
    // operation-fragment plan shape from committed migration SQL.
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

    // Reconstruct canonical fragments from committed SQL so replay
    // checksums and runner checks align for composed migrations.
    let fallback =
        djogi::migrate::canonical_fallback_replay_plan(bucket, version, &up_sql, &down_sql)
            .map_err(|e| match e {
                djogi::migrate::FallbackReplayPlanError::NonTransactionalStatement { shape } => {
                    ApplyReplayPlanError::NonTransactionalWithoutReplayPlan {
                        shape,
                        path: replay_plan_path.clone(),
                    }
                }
            })?;

    // Up checksum mismatch means the file changed after compose and the
    // pending row was generated from different up SQL.
    if fallback.checksum_up != pending_checksum_up {
        return Err(ApplyReplayPlanError::FallbackChecksumMismatch {
            side: "up",
            computed: fallback.checksum_up,
            pending: pending_checksum_up.to_string(),
        });
    }

    // Down checksum mismatch means the .down.sql file changed after compose.
    // The sidecar path (lines 131-134) already checks both sides; the fallback
    // path must apply the same guard so a post-compose edit to the down file
    // is caught consistently.
    if fallback.checksum_down.as_deref() != pending_checksum_down {
        return Err(ApplyReplayPlanError::FallbackChecksumMismatch {
            side: "down",
            computed: fallback.checksum_down.unwrap_or_default(),
            pending: pending_checksum_down.unwrap_or_default().to_string(),
        });
    }

    Ok((fallback.plan, fallback.checksum_up, fallback.checksum_down))
}

/// Errors from [`load_replay_plan_from_disk`].
#[derive(Debug)]
enum ApplyReplayPlanError {
    PlanRead {
        path: PathBuf,
        source: String,
    },
    Parse {
        path: PathBuf,
        source: String,
    },
    FormatVersion {
        found: String,
        path: PathBuf,
    },
    ChecksumMismatch,
    NonTransactionalWithoutReplayPlan {
        shape: &'static str,
        path: PathBuf,
    },
    SqlRead {
        path: PathBuf,
        source: String,
    },
    FallbackChecksumMismatch {
        side: &'static str,
        computed: String,
        pending: String,
    },
}

impl std::fmt::Display for ApplyReplayPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PlanRead { path, source } => {
                write!(f, "read replay plan {}: {source}", path.display())
            }
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
            Self::NonTransactionalWithoutReplayPlan { shape, path } => write!(
                f,
                "migration contains `{shape}`, which cannot replay as a single \
                 transaction and requires its committed replay plan; restore {} \
                 (or re-run `djogi migrations compose`) and retry",
                path.display()
            ),
            Self::SqlRead { path, source } => {
                write!(f, "read SQL file {}: {source}", path.display())
            }
            Self::FallbackChecksumMismatch {
                side,
                computed,
                pending,
            } => write!(
                f,
                "committed {side} SQL checksum {computed} does not match the pending \
                 plan's {pending}; the file changed after compose — re-run \
                 `djogi migrations compose` (or restore the committed file)"
            ),
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
            for bucket in &report.converged_snapshot_buckets {
                println!(
                    "snapshot converged: {database}/{app} — snapshot updated to scoped enum set, no migration needed",
                    database = bucket.database,
                    app = if bucket.app.is_empty() {
                        "_global_"
                    } else {
                        bucket.app.as_str()
                    },
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
        Err(e) => {
            eprintln!("djogi migrations compose: {e}");
            ExitCode::from(compose_error_exit_code(&e) as u8)
        }
    }
}

/// Map a [`ComposeError`] to an exit code for compose command handling.
///
/// Exit-code contract (mirrors `docs/spec/migrations.md` §10.2):
/// - `0` — `NothingToCompose`: no delta for any bucket; not an error.
/// - `2` — operator-actionable refusal: the operator must fix a model
///   descriptor, resolve a conflict, or hand-write a migration, then
///   re-run. Covers both the flat top-level refusals AND the
///   operator-actionable sub-variants nested inside the `SqlEmit`,
///   `Diff`, and `PhaseZeroAutoEmit` wrappers — those wrappers do not
///   map to a single exit code, so each is destructured.
/// - `1` — runtime / framework-internal failure that is not the
///   operator's to fix by editing schema (transient I/O, serialization,
///   DB errors, framework routing-invariant violations). Retryable in CI.
///
/// `NothingToCompose` is mapped explicitly even though the compose
/// command intercepts it upstream and returns exit 0 before reaching
/// this helper — so the helper's contract is total and self-consistent
/// rather than relying on the call-site short-circuit.
fn compose_error_exit_code(error: &ComposeError) -> i32 {
    match error {
        ComposeError::NothingToCompose => 0,

        // Flat operator-actionable refusals.
        ComposeError::DestructiveRequiresAllowDestructive { .. }
        | ComposeError::TombstonedAppRequiresAllowDestructive { .. }
        | ComposeError::UnsupportedDelta { .. }
        | ComposeError::HandEditedMigrationWouldBeOverwritten { .. }
        | ComposeError::PendingJsonWouldBeOverwritten { .. }
        | ComposeError::FolderRenameTargetCollision { .. }
        | ComposeError::LinkageDropWithoutModels { .. }
        | ComposeError::CrossBucketForeignKeyCycle { .. } => 2,

        // The SQL emitter's failures split: some are operator-actionable
        // (schema transition cannot be auto-lowered, partition shape
        // change, malformed storage params), one is a framework routing
        // invariant, and one forwards a `DiffError` to be classified by
        // the shared `diff_exit_code` mapping.
        ComposeError::SqlEmit(e) => match e {
            SqlEmitError::Unsupported { .. }
            | SqlEmitError::UnsupportedPartitionChange { .. }
            | SqlEmitError::InvalidStorageParams { .. } => 2,
            SqlEmitError::Diff(diff_err) => diff_exit_code(diff_err),
            // PK-type flip reached the standard emitter — a framework
            // routing-invariant violation, not an operator-caused refusal.
            SqlEmitError::PkTypeFlipMustRouteToT9 { .. } => 1,
        },

        // Differential failures classified by the shared mapping.
        ComposeError::Diff(e) => diff_exit_code(e),

        // Phase-zero bootstrap auto-emit splits: invalid / unknown
        // extension names are operator-actionable; DB, I/O, and
        // serialization failures are transient runtime failures.
        ComposeError::PhaseZeroAutoEmit(e) => match e {
            AutoEmitError::Compose(BootstrapError::InvalidExtensionName { .. })
            | AutoEmitError::Compose(BootstrapError::UnknownExtension { .. }) => 2,
            AutoEmitError::Compose(BootstrapError::Db { .. })
            | AutoEmitError::Io { .. }
            | AutoEmitError::PendingJson(_) => 1,
        },

        // Runtime failures — transient or serialization, not the
        // operator's to fix by editing schema.
        ComposeError::Io { .. } | ComposeError::SerializeFailed(_) => 1,

        // Future operator-actionable variants MUST be added to the exit-2
        // arm above; this wildcard covers only the `#[non_exhaustive]`
        // safety requirement.
        _ => 1,
    }
}

/// Map a [`DiffError`] to a compose exit code.
///
/// Shared by [`compose_error_exit_code`] for both the top-level
/// `ComposeError::Diff` wrapper and the `SqlEmit(SqlEmitError::Diff(..))`
/// path, which forwards the same `DiffError` through the emitter layer.
/// `DiffError` is not `#[non_exhaustive]`, so this match is total — a new
/// variant becomes a compile error here, forcing a deliberate classification.
fn diff_exit_code(error: &DiffError) -> i32 {
    match error {
        // Operator must restructure the FK graph / resolve the
        // partitioned-parent combination before re-running.
        DiffError::PkFlipCascadeDepthExceeded { .. }
        | DiffError::PartitionedMultiParentClusterUnsupported { .. } => 2,
        // Framework-internal invariant violation (sidecar metadata out
        // of sync) — not an operator-fixable schema condition.
        DiffError::PkFlipMalformedSelfFkMetadata(_) => 1,
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
                match &e {
                    RunnerError::DriftDetected { bucket, report } => {
                        for line in render_drift_refusal(bucket, report) {
                            eprintln!("{line}");
                        }
                    }
                    _ => eprintln!(
                        "djogi migrations apply: runner error on {bucket_database}/{app_label}: {e}"
                    ),
                }
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
    /// Runner error — exit code 2 for operator-actionable refusals,
    /// 1 for retryable/internal failures (see [`runner_error_exit_code`]).
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

    let snap_path = reconstruct_snapshot_path(workspace, &bucket);
    let drift_baseline = load_drift_baseline(mode, &snap_path);

    // 4. Construct RunnerCtx.
    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: pending.version.clone(),
        description: pending.slug.clone(),
        checksum_up,
        checksum_down,
        snapshot: Some(pending.model_snapshot.clone()),
        snapshot_path: Some(snap_path),
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
        drift_baseline,
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

/// Map a [`RunnerError`] onto the CLI exit-code contract
/// (`docs/spec/configuration.md` §CLI Exit-Code Matrix): exit `2` for
/// operator-actionable refusals (deterministic — a blind retry hits the
/// same condition), exit `1` for transient / internal failures (CI may
/// retry). The per-variant classification lives on the enum itself
/// ([`RunnerError::is_operator_actionable`]) as a same-crate exhaustive
/// match, so every future variant is classified at compile time — this
/// mapper cannot silently bucket a new refusal as exit `1`, and the apply
/// and baseline paths cannot diverge (both delegate here).
fn runner_error_exit_code(error: &RunnerError) -> i32 {
    if error.is_operator_actionable() { 2 } else { 1 }
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

/// Resolve the apply-time drift baseline from the recorded snapshot.
/// Every on-disk state maps to a typed [`DriftBaseline`] — never an error:
/// - `--fake` apply disables the gate ([`DriftBaseline::Disabled`]).
/// - A readable snapshot becomes [`DriftBaseline::Snapshot`].
/// - A missing file becomes [`DriftBaseline::Missing`].
/// - A present-but-unreadable file becomes [`DriftBaseline::Corrupted`],
///   carrying the parse/IO error text.
///
/// The runner decides whether each non-`Disabled` state refuses (the bucket
/// has applied history) or self-skips (the bucket was never applied), so this
/// loader must not collapse a corrupt snapshot into a hard error — doing so
/// would refuse even a never-applied bucket, where drift is undefined.
fn load_drift_baseline(mode: &FakeMode, snap_path: &Path) -> DriftBaseline {
    if let FakeMode::Fake { .. } = mode {
        return DriftBaseline::Disabled;
    }
    match load_snapshot(snap_path) {
        Ok(snapshot) => DriftBaseline::Snapshot(snapshot),
        Err(SnapshotError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            DriftBaseline::Missing
        }
        Err(e) => DriftBaseline::Corrupted(e.to_string()),
    }
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

fn render_drift_refusal(report_bucket: &BucketKey, report: &VerifyReport) -> Vec<String> {
    let mut lines = render_verify_report(report, report_bucket);
    lines.push(String::new());
    lines.push(
        "Apply refused before any migration SQL ran because error-severity drift was detected."
            .to_string(),
    );
    lines.push(
        "Next steps: inspect with `djogi migrations verify`, reconcile intentional drift \
         with `djogi migrations attune`, or if drift is from partial non-transactional \
         progress, resume with `djogi migrations repair resume-partial`."
            .to_string(),
    );
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
        | RepairError::Runner(..)                     // runner-level failure during repair (identity binding, leaf-identity probe, plan materialization); wraps a transient SQL/connection/catalog source
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

/// Map a [`RollbackError`] onto the CLI exit-code contract: exit `2` for
/// deterministic operator-actionable refusals, exit `1` for transient or
/// runtime failures.
fn rollback_error_exit_code(error: &RollbackError) -> i32 {
    match error {
        RollbackError::Runner { source, .. } => runner_error_exit_code(source),
        RollbackError::LossyRollbackRefused { .. }
        | RollbackError::VersionNotRollbackable { .. }
        | RollbackError::VersionNotFound { .. }
        | RollbackError::BucketAppMismatch { .. }
        | RollbackError::ChecksumDrift { .. }
        | RollbackError::PriorSnapshotMissing
        | RollbackError::LeafIdentityMismatch { .. }
        | RollbackError::StalePhaseZeroDown { .. }
        // If SnapshotPersistFailed ever fires the rollback's SQL already
        // committed; the live DB advanced but the snapshot is stale. A blind
        // retry would refuse (the ledger row is no longer rollbackable), so
        // this is an operator-actionable repair signal, not a transient
        // failure — exit 2, pointing at snapshot-rebuild.
        | RollbackError::SnapshotPersistFailed { .. }
        | RollbackError::MissingRollbackIdentity { .. } => 2,
        RollbackError::DownStatementFailed { .. } => 1,
    }
}

/// `djogi migrations rollback` entry point.
#[allow(clippy::too_many_arguments)]
pub fn rollback_cmd(
    to: Option<String>,
    dry_run: bool,
    allow_data_loss: bool,
    reason: Option<String>,
    app: Option<&str>,
    database: Option<&str>,
    workspace: Option<PathBuf>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> ExitCode {
    if allow_data_loss {
        match reason.as_deref() {
            Some(reason) if !reason.trim().is_empty() => {}
            Some(_) => {
                eprintln!(
                    "djogi migrations rollback --allow-data-loss: --reason must not be empty; \
                     supply a non-empty reason why lossy rollback is acceptable"
                );
                return ExitCode::from(2);
            }
            None => {
                eprintln!(
                    "djogi migrations rollback --allow-data-loss: --reason is required; \
                     supply a reason why lossy rollback is acceptable. \
                     This is recorded in the ledger audit trail."
                );
                return ExitCode::from(2);
            }
        }
    }

    let workspace = resolve_workspace(workspace);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi migrations rollback: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let exit = runtime.block_on(async {
        run_rollback(
            &workspace,
            to.as_deref(),
            dry_run,
            allow_data_loss,
            reason.as_deref(),
            app,
            database,
            node_id,
            single_node_dev,
        )
        .await
    });
    ExitCode::from(exit as u8)
}

#[allow(clippy::too_many_arguments)]
async fn run_rollback(
    workspace: &Path,
    to: Option<&str>,
    dry_run: bool,
    allow_data_loss: bool,
    reason: Option<&str>,
    app: Option<&str>,
    database: Option<&str>,
    node_id: Option<u32>,
    single_node_dev: bool,
) -> i32 {
    use djogi::config::DjogiConfig;

    let config = match DjogiConfig::load_from_workspace(workspace) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("djogi migrations rollback: config load: {e}");
            return 1;
        }
    };

    let runner_identity: Option<djogi::migrate::RunnerIdentity> = if dry_run {
        None
    } else {
        match crate::identity::resolve_identity(
            node_id,
            single_node_dev,
            &config.profile,
            "rollback",
        ) {
            Ok(resolved) => Some(resolved.into_runner_identity()),
            Err(e) => {
                let _ = crate::identity::print_identity_error("rollback", &e);
                return 2;
            }
        }
    };

    let db_name = resolve_database(database, &config);
    let url = match resolve_bucket_url(&config.database, &db_name) {
        Some(url) => url,
        None => {
            eprintln!("djogi migrations rollback: cannot derive a database URL for `{db_name}`");
            return 2;
        }
    };

    let mut ctx = match connect_and_check(&url).await {
        ContextOutcome::Ready(ctx) => ctx,
        ContextOutcome::UnsupportedVersion(e) => {
            crate::print_support_boundary_error("migrations rollback", &e);
            return 2;
        }
        ContextOutcome::RuntimeError(msg) => {
            eprintln!("djogi migrations rollback: pool: {msg}");
            return 1;
        }
    };

    let app_label = app.unwrap_or("");
    let bucket = BucketKey {
        database: db_name,
        app: app_label.to_string(),
    };

    let pre_lock_rows = match read_ledger_rows_or_empty(&mut ctx).await {
        Ok(rows) => rows,
        Err(msg) => {
            eprintln!("djogi migrations rollback: ledger read: {msg}");
            return 1;
        }
    };
    let pre_lock_targets = match select_rollback_targets(&pre_lock_rows, app_label, to) {
        Ok(targets) => targets,
        Err(msg) => {
            eprintln!("djogi migrations rollback: {msg}");
            return 2;
        }
    };
    if pre_lock_targets.is_empty() {
        println!("Nothing to roll back.");
        return 0;
    }

    if dry_run {
        let gated_targets = match gate_rollback_targets(workspace, &bucket, &pre_lock_targets) {
            Ok(targets) => targets,
            Err(RollbackCliGateError::Refusal(msg)) => {
                eprintln!("djogi migrations rollback: {msg}");
                return 2;
            }
            Err(RollbackCliGateError::Io(msg)) => {
                eprintln!("djogi migrations rollback: {msg}");
                return 1;
            }
        };
        if !allow_data_loss && let Some((version, markers)) = first_lossy_target(&gated_targets) {
            eprintln!("djogi migrations rollback: rollback refused for `{version}`:");
            for marker in markers {
                eprintln!("  {marker}");
            }
            eprintln!("pass --allow-data-loss with --reason to proceed");
            return 2;
        }
        print_rollback_data_loss_warning();
        for target in &gated_targets {
            println!(
                "-- rollback {} ({}/{})",
                target.row.version,
                bucket.database,
                display_bucket_app(&bucket.app)
            );
            print!("{}", target.down_sql);
            if !target.down_sql.ends_with('\n') {
                println!();
            }
        }
        println!(
            "preview of the current ledger state; the real run re-reads the ledger under the workspace lock"
        );
        println!("dry run — nothing executed.");
        return 0;
    }

    let lock_path = workspace.join(LOCK_FILE_NAME);
    let guard = match acquire_workspace_lock(&lock_path, GUARD_DEFAULT_TIMEOUT) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("djogi migrations rollback: workspace lock: {e}");
            return 1;
        }
    };

    let locked_rows = match read_ledger_rows_or_empty(&mut ctx).await {
        Ok(rows) => rows,
        Err(msg) => {
            eprintln!("djogi migrations rollback: ledger read: {msg}");
            return 1;
        }
    };
    let locked_targets = match select_rollback_targets(&locked_rows, app_label, to) {
        Ok(targets) => targets,
        Err(msg) => {
            eprintln!("djogi migrations rollback: {msg}");
            return 2;
        }
    };
    if let Err(msg) = ensure_no_target_drift(&pre_lock_targets, &locked_targets) {
        eprintln!("djogi migrations rollback: {msg}");
        return 2;
    }

    let gated_targets = match gate_rollback_targets(workspace, &bucket, &locked_targets) {
        Ok(targets) => targets,
        Err(RollbackCliGateError::Refusal(msg)) => {
            eprintln!("djogi migrations rollback: {msg}");
            return 2;
        }
        Err(RollbackCliGateError::Io(msg)) => {
            eprintln!("djogi migrations rollback: {msg}");
            return 1;
        }
    };

    if !allow_data_loss && let Some((version, markers)) = first_lossy_target(&gated_targets) {
        eprintln!("djogi migrations rollback: rollback refused for `{version}`:");
        for marker in markers {
            eprintln!("  {marker}");
        }
        eprintln!("pass --allow-data-loss with --reason to proceed");
        return 2;
    }

    print_rollback_data_loss_warning();

    let audit_pool = match djogi::migrate::resolve_audit_url(&config) {
        Ok(url) => djogi::migrate::build_audit_pool(&url).await.ok(),
        Err(_) => None,
    };
    let mut rolled_back_count = 0usize;
    let mut loop_failure: Option<djogi::migrate::RollbackError> = None;
    let lossy_reason = reason.map(str::to_string);

    for target in gated_targets {
        let plan = djogi::migrate::MigrationPlan {
            bucket: bucket.clone(),
            classification: djogi::migrate::Classification::Additive,
            segments: vec![djogi::migrate::Segment {
                kind: djogi::migrate::SegmentKind::Transactional,
                statements: vec![djogi::migrate::OperationSql {
                    label: format!("rollback {}", target.row.version),
                    up: target.up_sql.clone(),
                    down: target.down_sql.clone(),
                    lossy: None,
                }],
            }],
        };
        let runner_ctx = RunnerCtx {
            bucket: bucket.clone(),
            version: target.row.version.clone(),
            description: target.row.description.clone(),
            checksum_up: target.checksum_up.clone(),
            checksum_down: target.checksum_down.clone(),
            snapshot: None,
            snapshot_path: None,
            config: djogi::config::MigrateConfig {
                concurrent_warn_relpages: config.migrate.concurrent_warn_relpages,
                strict_concurrent_warnings: config.migrate.strict_concurrent_warnings,
                pk_flip_long_tx_threshold_secs: config.migrate.pk_flip_long_tx_threshold_secs,
                pk_flip_join_table_option: config.migrate.pk_flip_join_table_option,
            },
            out_of_order_policy: djogi::migrate::OutOfOrderPolicy::default_for_config(&config),
            audit_pool: audit_pool.clone(),
            runner_identity,
            // Apply-time drift verification is an apply-path gate;
            // rollback_plan never reads this field. Rollback's own
            // pre-execution guard is the ledger-vs-file checksum-parity
            // gate inside rollback_handle_lock.
            drift_baseline: DriftBaseline::Disabled,
        };
        let policy = match lossy_reason.as_deref() {
            Some(reason) => djogi::migrate::LossyRollbackPolicy::Allow {
                reason: reason.to_string(),
            },
            None => djogi::migrate::LossyRollbackPolicy::Refuse,
        };

        println!("  rolling back {}...", target.row.version);
        match djogi::migrate::rollback_plan(&mut ctx, &plan, &runner_ctx, &guard, policy, None)
            .await
        {
            Ok(report) => {
                if let Some(lossy_reason) = report.lossy_reason.as_deref() {
                    println!(
                        "  rolled back {} (lossy reason: {lossy_reason})",
                        target.row.version
                    );
                } else {
                    println!("  rolled back {}", target.row.version);
                }
                rolled_back_count += 1;
            }
            Err(e) => {
                eprintln!("djogi migrations rollback: {e}");
                loop_failure = Some(e);
                break;
            }
        }
    }

    let snapshot_path = reconstruct_snapshot_path(workspace, &bucket);
    let live_db_mutated = rolled_back_count > 0
        || loop_failure
            .as_ref()
            .is_some_and(djogi::migrate::RollbackError::live_db_committed);

    match loop_failure {
        None => {
            match repair_snapshot_rebuild(
                &mut ctx,
                &guard,
                &bucket,
                &snapshot_path,
                RepairConfirmation::OperatorAcknowledged,
            )
            .await
            {
                Ok(_) => {
                    println!(
                        "rolled back {rolled_back_count} migration(s); snapshot re-projected."
                    );
                    0
                }
                Err(e) => {
                    eprintln!(
                        "djogi migrations rollback: rollback recorded; snapshot rebuild failed: {e} — run `djogi migrations repair snapshot-rebuild --app {} --database {}` to restore the snapshot",
                        bucket.app, bucket.database,
                    );
                    2
                }
            }
        }
        Some(e) if live_db_mutated => {
            match repair_snapshot_rebuild(
                &mut ctx,
                &guard,
                &bucket,
                &snapshot_path,
                RepairConfirmation::OperatorAcknowledged,
            )
            .await
            {
                Ok(_) => {
                    println!("snapshot re-projected to match committed rollback work.");
                }
                Err(rebuild_error) => {
                    eprintln!(
                        "djogi migrations rollback: snapshot may be stale: {rebuild_error} — run `djogi migrations repair snapshot-rebuild --app {} --database {}` to restore the snapshot",
                        bucket.app, bucket.database,
                    );
                }
            }
            rollback_error_exit_code(&e)
        }
        Some(e) => rollback_error_exit_code(&e),
    }
}

/// Compute the ordered rollback set for one bucket from the full ledger
/// listing. Pure — no I/O — so the walk rules are unit-testable.
fn select_rollback_targets<'a>(
    rows: &'a [djogi::migrate::LedgerSummaryRow],
    app_label: &str,
    to: Option<&str>,
) -> Result<Vec<&'a djogi::migrate::LedgerSummaryRow>, String> {
    use djogi::migrate::LedgerStatus;

    let mut bucket_rows: Vec<&djogi::migrate::LedgerSummaryRow> = rows
        .iter()
        .filter(|row| row.app_label == app_label)
        .collect();
    bucket_rows.sort_by_key(|row| std::cmp::Reverse(row.id));

    let floor_id = match to {
        None => None,
        Some(version) => {
            let target = bucket_rows
                .iter()
                .find(|row| row.version == version)
                .ok_or_else(|| {
                    format!("--to version `{version}` is not present in this bucket's ledger")
                })?;
            if !matches!(
                target.status,
                LedgerStatus::Applied | LedgerStatus::Faked | LedgerStatus::Baseline
            ) {
                return Err(format!(
                    "--to version `{version}` has status `{status}`; the rollback \
                     target must remain applied (applied / faked / baseline)",
                    status = target.status.as_db_str(),
                ));
            }
            Some(target.id)
        }
    };

    let mut targets = Vec::new();
    for row in &bucket_rows {
        if let Some(floor) = floor_id
            && row.id <= floor
        {
            break;
        }
        match row.status {
            LedgerStatus::RolledBack => continue,
            LedgerStatus::Applied | LedgerStatus::Faked => {
                targets.push(*row);
                if floor_id.is_none() {
                    break;
                }
            }
            LedgerStatus::Pending | LedgerStatus::Failed => {
                return Err(format!(
                    "ledger row `{version}` has status `{status}`; resolve it with \
                     `djogi migrations repair` before rolling back past it",
                    version = row.version,
                    status = row.status.as_db_str(),
                ));
            }
            LedgerStatus::Baseline => {
                if floor_id.is_none() {
                    break;
                }
                return Err(format!(
                    "cannot roll back past baseline row `{version}`",
                    version = row.version,
                ));
            }
        }
    }

    Ok(targets)
}

/// Refuse when the rollback target set computed under the workspace lock
/// differs from the pre-lock baseline the operator may have just observed.
fn ensure_no_target_drift(
    pre_lock: &[&djogi::migrate::LedgerSummaryRow],
    locked: &[&djogi::migrate::LedgerSummaryRow],
) -> Result<(), String> {
    // Drift detection compares (id, version, status) triples only —
    // LedgerSummaryRow carries no checksums, so committed-SQL checksum parity
    // is not (and need not be) checked here. That parity is enforced
    // downstream by rollback_plan's ChecksumDrift refusal before any down SQL
    // runs; this guard only catches the ledger SET changing between the
    // pre-lock read and the locked read.
    let key = |set: &[&djogi::migrate::LedgerSummaryRow]| -> Vec<(i64, String, String)> {
        set.iter()
            .map(|row| {
                (
                    row.id,
                    row.version.clone(),
                    row.status.as_db_str().to_string(),
                )
            })
            .collect()
    };
    if key(pre_lock) != key(locked) {
        return Err(
            "ledger changed while waiting for the workspace lock; rerun the command".to_string(),
        );
    }
    Ok(())
}

/// Collect the lossy-marker comment lines from a committed down SQL file.
fn scan_lossy_down_markers(down_sql: &str) -> Vec<String> {
    down_sql
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("-- LOSSY"))
        .map(str::to_string)
        .collect()
}

#[derive(Debug)]
enum RollbackCliGateError {
    Refusal(String),
    Io(String),
}

#[derive(Debug)]
struct GatedRollbackTarget<'a> {
    row: &'a djogi::migrate::LedgerSummaryRow,
    up_sql: String,
    down_sql: String,
    checksum_up: String,
    checksum_down: Option<String>,
    lossy_markers: Vec<String>,
}

async fn read_ledger_rows_or_empty(
    ctx: &mut djogi::context::DjogiContext,
) -> Result<Vec<djogi::migrate::LedgerSummaryRow>, String> {
    match djogi::migrate::select_all_ledger_rows(ctx).await {
        Ok(rows) => Ok(rows),
        Err(e) => {
            if e.to_string().contains("djogi_schema_migrations") {
                Ok(Vec::new())
            } else {
                Err(e.to_string())
            }
        }
    }
}

fn gate_rollback_targets<'a>(
    workspace: &Path,
    bucket: &BucketKey,
    rows: &[&'a djogi::migrate::LedgerSummaryRow],
) -> Result<Vec<GatedRollbackTarget<'a>>, RollbackCliGateError> {
    let bucket_dir = djogi::migrate::bucket_dir(workspace, bucket);
    let mut gated = Vec::with_capacity(rows.len());

    for row in rows {
        let down_path = bucket_dir.join(djogi::migrate::down_filename(&row.version));
        let down_sql = match std::fs::read_to_string(&down_path) {
            Ok(sql) => sql,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(RollbackCliGateError::Refusal(format!(
                    "version `{}` has no committed down file; use `djogi migrations repair partial-apply {} rolled-back` if the rollback already happened out of band",
                    row.version, row.version
                )));
            }
            Err(e) => {
                return Err(RollbackCliGateError::Io(format!(
                    "read down SQL {}: {e}",
                    down_path.display()
                )));
            }
        };
        let checksum_down = djogi::migrate::compute_committed_down_sql_checksum(&down_sql);
        if checksum_down.is_none() {
            return Err(RollbackCliGateError::Refusal(format!(
                "version `{}` has no executable down SQL; use `djogi migrations repair partial-apply {} rolled-back` if the rollback already happened out of band",
                row.version, row.version
            )));
        }
        if let Some(shape) = djogi::migrate::find_non_transactional_statement_shape(&down_sql) {
            return Err(RollbackCliGateError::Refusal(format!(
                "version `{}` contains non-transactional down SQL (`{shape}`); use the library rollback entry point for this migration",
                row.version
            )));
        }

        let up_path = bucket_dir.join(djogi::migrate::up_filename(&row.version));
        let up_sql = std::fs::read_to_string(&up_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RollbackCliGateError::Refusal(format!(
                    "version `{}` is missing its committed up file",
                    row.version
                ))
            } else {
                RollbackCliGateError::Io(format!("read up SQL {}: {e}", up_path.display()))
            }
        })?;
        let checksum_up = djogi::migrate::compute_committed_sql_checksum(
            &up_sql,
            djogi::migrate::ResetSqlSide::Up,
        );
        let lossy_markers = scan_lossy_down_markers(&down_sql);

        gated.push(GatedRollbackTarget {
            row,
            up_sql,
            down_sql,
            checksum_up,
            checksum_down,
            lossy_markers,
        });
    }

    Ok(gated)
}

fn first_lossy_target<'a>(
    targets: &'a [GatedRollbackTarget<'a>],
) -> Option<(&'a str, &'a [String])> {
    targets
        .iter()
        .find(|target| !target.lossy_markers.is_empty())
        .map(|target| (target.row.version.as_str(), target.lossy_markers.as_slice()))
}

fn display_bucket_app(app_label: &str) -> &str {
    if app_label.is_empty() {
        "_global_"
    } else {
        app_label
    }
}

fn print_rollback_data_loss_warning() {
    eprintln!(
        "WARNING: rollback executes committed down SQL and may permanently remove data or schema state."
    );
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
        drift_baseline: DriftBaseline::Disabled,
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
            runner_error_exit_code(&e)
        }
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

    fn ledger_row(
        id: i64,
        version: &str,
        status: LedgerStatus,
        app_label: &str,
    ) -> djogi::migrate::LedgerSummaryRow {
        djogi::migrate::LedgerSummaryRow {
            id,
            version: version.to_string(),
            description: format!("desc {version}"),
            status,
            execution_time_ms: 0,
            applied_at_rfc3339: "2026-01-01T00:00:00Z".to_string(),
            applied_by: "test".to_string(),
            run_id: id,
            partial_apply_note: None,
            app_label: app_label.to_string(),
            out_of_order_flag: false,
        }
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

    #[serial_test::serial]
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

    fn write_fallback_migration_files(
        work: &std::path::Path,
        version: &str,
        up: &str,
        down: Option<&str>,
    ) -> djogi::migrate::BucketKey {
        let bucket = djogi::migrate::BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let dir = djogi::migrate::bucket_dir(work, &bucket);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(djogi::migrate::up_filename(version)), up).unwrap();
        if let Some(down) = down {
            fs::write(dir.join(djogi::migrate::down_filename(version)), down).unwrap();
        }
        bucket
    }

    const COMPOSED_UP_FIXTURE: &str = "-- Djogi composed migration — up\n\
                                              -- Version: V20260612000000__add_widgets\n\
                                              -- Bucket:  main/_global_\n\
                                              -- Classification: Additive\n\
                                              --\n\
                                              -- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\
                                              \n\
                                              -- CreateModel widgets\n\
                                              CREATE TABLE \"widgets\" (\"id\" BIGINT PRIMARY KEY);\n\
                                              \n\
                                              -- AddIndex widgets_id_idx\n\
                                              CREATE INDEX \"widgets_id_idx\" ON \"widgets\" (\"id\");\n\
                                              \n";

    const COMPOSED_DOWN_FIXTURE: &str = "-- Djogi composed migration — down\n\
                                                -- Version: V20260612000000__add_widgets\n\
                                                -- Bucket:  main/_global_\n\
                                                --\n\
                                                -- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\
                                                \n\
                                                -- AddIndex widgets_id_idx\n\
                                                DROP INDEX \"widgets_id_idx\";\n\
                                                \n\
                                                -- CreateModel widgets\n\
                                                DROP TABLE \"widgets\";\n\
                                                \n";

    #[test]
    fn fallback_checksums_match_canonical_domain_for_composed_file() {
        let work = temp_workspace("fallback-canonical");
        let version = "V20260612000000__add_widgets";
        let bucket = write_fallback_migration_files(
            &work,
            version,
            COMPOSED_UP_FIXTURE,
            Some(COMPOSED_DOWN_FIXTURE),
        );
        let canonical_up = djogi::migrate::compute_committed_sql_checksum(
            COMPOSED_UP_FIXTURE,
            djogi::migrate::ResetSqlSide::Up,
        );
        let canonical_down =
            djogi::migrate::compute_committed_down_sql_checksum(COMPOSED_DOWN_FIXTURE);

        let (plan, checksum_up, checksum_down) = load_replay_plan_from_disk(
            &work,
            &bucket,
            version,
            &canonical_up,
            canonical_down.as_deref(),
        )
        .expect("fallback must load");

        assert_eq!(checksum_up, canonical_up);
        assert_eq!(checksum_down, canonical_down);
        assert!(checksum_down.is_some());

        let rehash = djogi::migrate::compute_checksum(
            plan.segments
                .iter()
                .flat_map(|segment| segment.statements.iter())
                .map(|stmt| stmt.up.as_str()),
        );
        assert_eq!(rehash, checksum_up);
    }

    #[test]
    fn fallback_down_checksum_none_for_comment_only_down() {
        let work = temp_workspace("fallback-comment-down");
        let version = "V20260612000001__no_rollback";
        let up = "CREATE TABLE plain (id BIGINT);\n";
        let bucket = write_fallback_migration_files(&work, version, up, Some("-- no rollback\n"));
        let pending_up = djogi::migrate::compute_checksum([up]);
        let (_, _, checksum_down) =
            load_replay_plan_from_disk(&work, &bucket, version, &pending_up, None).expect("load");
        assert_eq!(checksum_down, None);
    }

    #[test]
    fn fallback_down_checksum_some_for_executable_hand_authored_down() {
        let work = temp_workspace("fallback-plain-down");
        let version = "V20260612000002__plain";
        let up = "CREATE TABLE plain (id BIGINT);\n";
        let down = "DROP TABLE plain;\n";
        let bucket = write_fallback_migration_files(&work, version, up, Some(down));
        let pending_up = djogi::migrate::compute_checksum([up]);
        let pending_down = djogi::migrate::compute_checksum([down]);
        let (_, checksum_up, checksum_down) =
            load_replay_plan_from_disk(&work, &bucket, version, &pending_up, Some(&pending_down))
                .expect("load");
        assert_eq!(checksum_up, pending_up);
        assert_eq!(checksum_down, Some(pending_down));
    }

    #[test]
    fn fallback_refuses_non_transactional_shape_without_replay_plan() {
        let work = temp_workspace("fallback-nontx");
        let version = "V20260612000003__conc_idx";
        let up = "CREATE INDEX CONCURRENTLY widgets_idx ON widgets (id);";
        let bucket = write_fallback_migration_files(&work, version, up, None);
        let pending_up = djogi::migrate::compute_checksum([up]);
        let err = load_replay_plan_from_disk(&work, &bucket, version, &pending_up, None)
            .expect_err("non-transactional migrations should refuse fallback");
        let rendered = err.to_string();
        assert!(
            rendered.contains("CREATE INDEX CONCURRENTLY"),
            "actionable shape in: {rendered}"
        );
    }

    #[test]
    fn fallback_refuses_when_up_file_diverges_from_pending_checksum() {
        let work = temp_workspace("fallback-tamper");
        let version = "V20260612000004__tampered";
        let bucket = write_fallback_migration_files(&work, version, COMPOSED_UP_FIXTURE, None);
        let stale_pending = djogi::migrate::compute_checksum(["something else entirely"]);
        let err = load_replay_plan_from_disk(&work, &bucket, version, &stale_pending, None)
            .expect_err("up-domain mismatch must not be silently ignored");
        assert!(err.to_string().contains("checksum"), "actionable: {err}");
    }

    /// When the on-disk `.down.sql` file has changed since compose but the up
    /// checksum still matches, the fallback path must detect the drift and
    /// return an error — not silently record the new down checksum.  This is
    /// the parallel of `fallback_refuses_when_up_file_diverges_from_pending_checksum`
    /// for the down side.
    #[test]
    fn fallback_refuses_when_down_file_diverges_from_pending_checksum() {
        let work = temp_workspace("fallback-tamper-down");
        let version = "V20260612000004__tampered_down";
        let up = "CREATE TABLE downcheck (id BIGINT);\n";
        let original_down = "DROP TABLE downcheck;\n";
        let tampered_down = "DROP TABLE downcheck; -- tampered\n";

        // Write files with the tampered down content.
        let bucket = write_fallback_migration_files(&work, version, up, Some(tampered_down));

        // pending_checksum_down reflects the original down file (before tamper).
        let pending_up = djogi::migrate::compute_checksum([up]);
        let pending_down = djogi::migrate::compute_checksum([original_down]);

        // The function must not succeed: the on-disk down SQL no longer matches
        // the pending plan's checksum_down.
        let err =
            load_replay_plan_from_disk(&work, &bucket, version, &pending_up, Some(&pending_down))
                .expect_err("down-domain mismatch must not be silently ignored");

        let rendered = err.to_string();
        assert!(
            rendered.contains("checksum"),
            "error message must mention checksum: {rendered}"
        );
        assert!(
            rendered.contains("down"),
            "error message must identify the down side: {rendered}"
        );
    }

    #[test]
    fn fallback_unreadable_replay_plan_sidecar_is_an_error_not_a_silent_fallback() {
        let work = temp_workspace("fallback-badplan");
        let version = "V20260612000005__badplan";
        let bucket =
            write_fallback_migration_files(&work, version, "CREATE TABLE t (id BIGINT);\n", None);
        let plan_path =
            djogi::migrate::bucket_dir(&work, &bucket).join(format!("{version}.plan.json"));
        fs::create_dir_all(&plan_path).unwrap();
        let pending_up = djogi::migrate::compute_checksum(["CREATE TABLE t (id BIGINT);\n"]);
        load_replay_plan_from_disk(&work, &bucket, version, &pending_up, None)
            .expect_err("non-NotFound sidecar read errors must surface");
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
    #[serial_test::serial]
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
    #[serial_test::serial]
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

    #[serial_test::serial]
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
            // Emit Phase 0 so the runner's SingleNodeDev provisioning sets the
            // heer.node_id GUC on the migration session before binding node 1.
            // Phase 0 lands at PHASE_ZERO_VERSION (different from composed_version)
            // so the version-scoped ledger assertions below are unaffected.
            skip_phase_zero_auto_emit: false,
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

        // Override DATABASE_URL to the per-test database for the duration of
        // run_apply. load_from_workspace unconditionally replaces config.database.url
        // with DATABASE_URL when the env var is present, so without this the
        // migrations land in the admin database rather than the per-test one.
        // DatabaseUrlEnvGuard holds the process-wide env mutex and restores the
        // prior value on Drop. #[djogi_test] does not acquire this mutex, so
        // there is no deadlock risk.
        let db_url_guard = DatabaseUrlEnvGuard::new();
        db_url_guard.set(&test_db_url);

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
                        true, // single_node_dev: select SingleNodeDev identity so Phase 0 seeds + binds node 1
                    ))
            })
            .await
            .expect("spawn_blocking join")
        };

        // Restore DATABASE_URL before the assertions; the per-test ctx already
        // points at the correct database and does not use DATABASE_URL.
        drop(db_url_guard);

        assert_eq!(
            exit, 0,
            "apply should succeed (tables created in FK dependency order)"
        );

        // Assert 1: FK constraint exists on event_log → users.
        let fk_rows = ctx
            .raw_rows(
                "SELECT c.conname \
                 FROM pg_constraint c \
                 JOIN pg_class r ON r.oid = c.conrelid \
                 JOIN pg_class f ON f.oid = c.confrelid \
                 WHERE r.relname = $1 AND c.contype = 'f' AND f.relname = $2",
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

    /// End-to-end test: two buckets sharing an enum type (`mood`) plus
    /// a cross-bucket FK. Compose + apply, then assert exactly one `CREATE TYPE`
    /// in Postgres, both tables exist, and no error during apply.
    #[djogi::djogi_test]
    async fn shared_enum_multi_slice_applies(mut ctx: djogi::context::DjogiContext) {
        // Unique suffix for table names — avoids collisions when tests run in parallel.
        static E2E_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = E2E_COUNTER.fetch_add(1, Ordering::SeqCst);
        let posts_table = format!("e2e_posts_{n}");
        let comments_table = format!("e2e_comments_{n}");

        let work = temp_workspace("shared-enum-e2e");
        let guard = djogi::migrate::acquire_workspace_lock(
            &work.join(LOCK_FILE_NAME),
            std::time::Duration::from_secs(5),
        )
        .expect("lock workspace");

        // Construct models: alpha bucket (owns enum + posts table) +
        // beta bucket (references same enum + FK → alpha.posts).
        let mut models: std::collections::BTreeMap<
            djogi::migrate::BucketKey,
            djogi::migrate::AppliedSchema,
        > = std::collections::BTreeMap::new();

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        // Shared enum schema.
        let mood_enum = djogi::migrate::schema::EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        // Alpha: posts table with mood column (enum owner by alphabetical order).
        {
            let mut alpha_schema = djogi::migrate::AppliedSchema {
                djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                enums: std::collections::BTreeMap::new(),
                format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-06-10T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models: std::collections::BTreeMap::new(),
                registered_apps: vec!["alpha".to_string()],
            };
            alpha_schema
                .enums
                .insert("mood".to_string(), mood_enum.clone());
            alpha_schema.models.insert(
                posts_table.clone(),
                djogi::migrate::TableSchema {
                    app: Some("alpha".to_string()),
                    columns: vec![
                        djogi::migrate::ColumnSchema {
                            name: "id".to_string(),
                            sql_type: "BIGINT".to_string(),
                            nullable: false,
                            default_sql: Some("heerid_next_desc()".to_string()),
                            ..default_col()
                        },
                        djogi::migrate::ColumnSchema {
                            name: "mood".to_string(),
                            sql_type: "mood".to_string(),
                            nullable: true,
                            ..default_col()
                        },
                    ],
                    primary_key: djogi::migrate::PrimaryKeySchema {
                        columns: vec!["id".to_string()],
                        kind: djogi::migrate::PkKindSchema::HeerIdRecencyBiased,
                    },
                    table: posts_table.clone(),
                    ..default_table()
                },
            );
            models.insert(alpha_bucket.clone(), alpha_schema);
        }

        // Beta: comments table with mood column + FK → alpha.posts.
        {
            let mut beta_schema = djogi::migrate::AppliedSchema {
                djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                enums: std::collections::BTreeMap::new(),
                format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-06-10T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models: std::collections::BTreeMap::new(),
                registered_apps: vec!["beta".to_string()],
            };
            beta_schema.enums.insert("mood".to_string(), mood_enum);
            beta_schema.models.insert(
                comments_table.clone(),
                djogi::migrate::TableSchema {
                    app: Some("beta".to_string()),
                    columns: vec![
                        djogi::migrate::ColumnSchema {
                            name: "id".to_string(),
                            sql_type: "BIGINT".to_string(),
                            nullable: false,
                            default_sql: Some("heerid_next_desc()".to_string()),
                            ..default_col()
                        },
                        djogi::migrate::ColumnSchema {
                            name: "post_id".to_string(),
                            sql_type: "BIGINT".to_string(),
                            nullable: false,
                            foreign_key: Some(djogi::migrate::ForeignKeySchema {
                                deferrable: false,
                                initially_deferred: false,
                                on_delete: djogi::migrate::OnDeleteSchema::Restrict,
                                ref_column: "id".to_string(),
                                ref_table: posts_table.clone(),
                            }),
                            ..default_col()
                        },
                        djogi::migrate::ColumnSchema {
                            name: "author_mood".to_string(),
                            sql_type: "mood".to_string(),
                            nullable: true,
                            ..default_col()
                        },
                    ],
                    primary_key: djogi::migrate::PrimaryKeySchema {
                        columns: vec!["id".to_string()],
                        kind: djogi::migrate::PkKindSchema::HeerIdRecencyBiased,
                    },
                    table: comments_table.clone(),
                    ..default_table()
                },
            );
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Empty snapshots — fresh compose so differ sees all tables + enum as new.
        let mut snapshots = std::collections::BTreeMap::new();
        for bucket in [&alpha_bucket, &beta_bucket] {
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
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            djogi::migrate::AppLifecycle {
                label: "beta".into(),
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
            name: "shared-enum-multi-slice",
            allow_destructive: false,
            force_overwrite: false,
            now: time::OffsetDateTime::UNIX_EPOCH
                + time::Duration::days(19726)
                + time::Duration::seconds(0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: false,
        };

        let compose_report = djogi::migrate::compose(compose_req).expect("compose");
        assert!(
            !compose_report.composed_buckets.is_empty(),
            "compose should produce delta buckets"
        );

        // Release the workspace lock before driving run_apply.
        drop(guard);

        // Extract the composed version from the report.
        let composed_version = &compose_report.composed_buckets[0].version;

        // Construct per-test database URL from DATABASE_URL.
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

        let db_url_guard = DatabaseUrlEnvGuard::new();
        db_url_guard.set(&test_db_url);

        // Drive the apply loop through run_apply.
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
                        true, // single_node_dev
                    ))
            })
            .await
            .expect("spawn_blocking join")
        };

        drop(db_url_guard);

        assert_eq!(
            exit, 0,
            "apply should succeed (enum created once, tables in FK order)"
        );

        // Assert 1: Exactly one `mood` enum type in pg_type.
        let mood_count = ctx
            .raw_scalar::<i64>(
                "SELECT count(*) FROM pg_type WHERE typname = $1",
                &[&"mood"],
            )
            .await
            .expect("query pg_type for mood");
        assert_eq!(
            mood_count, 1,
            "exactly one mood enum type should exist in pg_type, got {mood_count}"
        );

        // Assert 2: Both tables exist.
        let table_count = ctx
            .raw_scalar::<i64>(
                "SELECT count(*) FROM pg_class WHERE relname = $1 OR relname = $2",
                &[&posts_table.as_str(), &comments_table.as_str()],
            )
            .await
            .expect("query pg_class for tables");
        assert_eq!(
            table_count, 2,
            "both tables should exist ({posts_table}, {comments_table}), got {table_count}"
        );

        // Assert 3: Ledger has exactly two rows for the composed version.
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
             (alpha + beta), got {} rows",
            ledger_rows.len()
        );

        let _ = fs::remove_dir_all(&work);
    }

    fn bucket_snapshot(
        app: &str,
        models: std::collections::BTreeMap<String, djogi::migrate::TableSchema>,
        indexes: Vec<djogi::migrate::IndexSchema>,
    ) -> djogi::migrate::AppliedSchema {
        djogi::migrate::AppliedSchema {
            djogi_version: env!("CARGO_PKG_VERSION").to_string(),
            enums: std::collections::BTreeMap::new(),
            format_version: djogi::migrate::SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-06-13T00:00:00Z".to_string(),
            indexes,
            models,
            registered_apps: vec![app.to_string()],
        }
    }

    fn simple_table(
        app: &str,
        table: &str,
        mut columns: Vec<djogi::migrate::ColumnSchema>,
    ) -> djogi::migrate::TableSchema {
        for column in &mut columns {
            if column.name == "id" && column.default_sql.is_none() {
                column.default_sql = Some("heerid_next_desc()".to_string());
            }
        }
        djogi::migrate::TableSchema {
            app: Some(app.to_string()),
            columns,
            primary_key: djogi::migrate::PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: djogi::migrate::PkKindSchema::HeerIdRecencyBiased,
            },
            table: table.to_string(),
            ..default_table()
        }
    }

    fn id_col() -> djogi::migrate::ColumnSchema {
        djogi::migrate::ColumnSchema {
            name: "id".to_string(),
            sql_type: "BIGINT".to_string(),
            nullable: false,
            default_sql: Some("heerid_next_desc()".to_string()),
            ..default_col()
        }
    }

    fn text_col(name: &str, nullable: bool) -> djogi::migrate::ColumnSchema {
        djogi::migrate::ColumnSchema {
            name: name.to_string(),
            sql_type: "TEXT".to_string(),
            nullable,
            ..default_col()
        }
    }

    fn bigint_fk_col(
        name: &str,
        ref_table: &str,
        on_delete: djogi::migrate::OnDeleteSchema,
    ) -> djogi::migrate::ColumnSchema {
        djogi::migrate::ColumnSchema {
            name: name.to_string(),
            sql_type: "BIGINT".to_string(),
            nullable: false,
            foreign_key: Some(djogi::migrate::ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete,
                ref_column: "id".to_string(),
                ref_table: ref_table.to_string(),
            }),
            ..default_col()
        }
    }

    fn btree_index(table: &str, name: &str, column: &str) -> djogi::migrate::IndexSchema {
        djogi::migrate::IndexSchema {
            extension_dependency: None,
            include: vec![],
            index_type: djogi::migrate::IndexTypeSchema::BTree,
            kind: djogi::migrate::IndexKindSchema::NonUnique,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: table.to_string(),
            target: djogi::migrate::IndexTargetSchema::Columns(vec![
                djogi::migrate::IndexColumnSchema {
                    name: column.to_string(),
                    nulls: djogi::migrate::IndexNullsOrderSchema::Default,
                    opclass: None,
                    order: djogi::migrate::IndexOrderSchema::Asc,
                },
            ]),
        }
    }

    async fn run_apply_in_test_db(
        ctx: &mut djogi::context::DjogiContext,
        work: &std::path::Path,
        mode: FakeMode,
    ) -> i32 {
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

        fs::write(
            work.join("Djogi.toml"),
            format!(
                "[database]\nurl = \"{test_db_url}\"\n\
                 max_connections = 1\ndev_mode = false\n\
                 [server]\nhost = \"127.0.0.1\"\nport = 8080\n"
            ),
        )
        .unwrap();

        let db_url_guard = DatabaseUrlEnvGuard::new();
        db_url_guard.set(&test_db_url);

        let exit = {
            let work = work.to_path_buf();
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime")
                    .block_on(run_apply(&work, &mode, None, true))
            })
            .await
            .expect("spawn_blocking join")
        };

        drop(db_url_guard);
        exit
    }

    fn compose_bucket_migration(
        work: &std::path::Path,
        bucket: &BucketKey,
        current: djogi::migrate::AppliedSchema,
        previous: djogi::migrate::AppliedSchema,
        name: &str,
        now: time::OffsetDateTime,
    ) -> djogi::migrate::ComposeReport {
        let guard = djogi::migrate::acquire_workspace_lock(
            &work.join(LOCK_FILE_NAME),
            std::time::Duration::from_secs(5),
        )
        .expect("lock workspace");

        let mut models = std::collections::BTreeMap::new();
        models.insert(bucket.clone(), current);

        let mut snapshots = std::collections::BTreeMap::new();
        snapshots.insert(bucket.clone(), previous);

        let apps = vec![djogi::migrate::AppLifecycle {
            label: bucket.app.clone(),
            database: bucket.database.clone(),
            renamed_from: None,
            tombstone: false,
        }];

        let report = djogi::migrate::compose(djogi::migrate::ComposeRequest {
            workspace_root: work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name,
            allow_destructive: false,
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: false,
        })
        .expect("compose");
        drop(guard);
        report
    }

    #[djogi::djogi_test]
    async fn apply_refuses_on_column_drift_before_second_migration(
        mut ctx: djogi::context::DjogiContext,
    ) {
        static DRIFT_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = DRIFT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_users_{n}");
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let work = temp_workspace("drift-column-refusal");

        let v1_models = std::collections::BTreeMap::from([(
            table.clone(),
            simple_table(&bucket.app, &table, vec![id_col(), text_col("name", false)]),
        )]);
        let v1_snapshot = bucket_snapshot(&bucket.app, v1_models.clone(), vec![]);
        let empty_snapshot =
            bucket_snapshot(&bucket.app, std::collections::BTreeMap::new(), vec![]);
        let first_report = compose_bucket_migration(
            &work,
            &bucket,
            v1_snapshot.clone(),
            empty_snapshot,
            "drift-column-v1",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19729),
        );
        assert!(
            !first_report.composed_buckets.is_empty(),
            "first compose must emit"
        );
        let first_version = first_report.composed_buckets[0].version.clone();

        let first_exit = run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await;
        assert_eq!(first_exit, 0, "initial apply must succeed");

        ctx.raw_execute(
            &format!("ALTER TABLE {table} RENAME COLUMN name TO full_name"),
            &[],
        )
        .await
        .expect("rename live column out of band");

        let v2_models = std::collections::BTreeMap::from([(
            table.clone(),
            simple_table(
                &bucket.app,
                &table,
                vec![id_col(), text_col("name", false), text_col("email", true)],
            ),
        )]);
        let v2_snapshot = bucket_snapshot(&bucket.app, v2_models, vec![]);
        let second_report = compose_bucket_migration(
            &work,
            &bucket,
            v2_snapshot,
            v1_snapshot,
            "drift-column-v2",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19730),
        );
        let second_version = second_report.composed_buckets[0].version.clone();

        let second_exit = run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await;
        assert_eq!(second_exit, 2, "drifted apply must refuse");

        let applied_count = ctx
            .raw_scalar::<i64>(
                "SELECT count(*) FROM djogi_schema_migrations \
                 WHERE version = $1 AND app_label = $2",
                &[&second_version, &bucket.app],
            )
            .await
            .expect("count second-version rows");
        assert_eq!(
            applied_count, 0,
            "refusal must not insert second ledger row"
        );

        let email_exists = ctx
            .raw_scalar::<bool>(
                "SELECT EXISTS(
                    SELECT 1
                    FROM information_schema.columns
                    WHERE table_name = $1 AND column_name = 'email'
                 )",
                &[&table],
            )
            .await
            .expect("check email column");
        assert!(!email_exists, "refusal must precede second migration SQL");

        let first_rows = ctx
            .raw_scalar::<i64>(
                "SELECT count(*) FROM djogi_schema_migrations \
                 WHERE version = $1 AND app_label = $2 AND status = 'applied'",
                &[&first_version, &bucket.app],
            )
            .await
            .expect("count first-version rows");
        assert_eq!(first_rows, 1, "baseline apply should remain intact");
    }

    #[djogi::djogi_test]
    async fn apply_refuses_on_missing_snapshot_for_applied_bucket(
        mut ctx: djogi::context::DjogiContext,
    ) {
        static DRIFT_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = DRIFT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_missing_snapshot_{n}");
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let work = temp_workspace("drift-missing-snapshot");

        let v1_models = std::collections::BTreeMap::from([(
            table.clone(),
            simple_table(&bucket.app, &table, vec![id_col(), text_col("name", false)]),
        )]);
        let v1_snapshot = bucket_snapshot(&bucket.app, v1_models.clone(), vec![]);
        let empty_snapshot =
            bucket_snapshot(&bucket.app, std::collections::BTreeMap::new(), vec![]);
        compose_bucket_migration(
            &work,
            &bucket,
            v1_snapshot.clone(),
            empty_snapshot,
            "drift-missing-v1",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19731),
        );
        assert_eq!(
            run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await,
            0
        );

        let v2_models = std::collections::BTreeMap::from([(
            table.clone(),
            simple_table(
                &bucket.app,
                &table,
                vec![id_col(), text_col("name", false), text_col("email", true)],
            ),
        )]);
        compose_bucket_migration(
            &work,
            &bucket,
            bucket_snapshot(&bucket.app, v2_models, vec![]),
            v1_snapshot,
            "drift-missing-v2",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19732),
        );

        let snap_path = reconstruct_snapshot_path(&work, &bucket);
        fs::remove_file(&snap_path).expect("delete recorded snapshot");

        let exit = run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await;
        assert_eq!(exit, 2, "missing baseline must refuse on applied bucket");
    }

    #[djogi::djogi_test]
    async fn apply_refuses_on_dropped_index_drift(mut ctx: djogi::context::DjogiContext) {
        static DRIFT_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = DRIFT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("drift_index_{n}");
        let index_name = format!("{table}_name_idx");
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let work = temp_workspace("drift-index-refusal");

        let base_table = simple_table(&bucket.app, &table, vec![id_col(), text_col("name", false)]);
        let v1_snapshot = bucket_snapshot(
            &bucket.app,
            std::collections::BTreeMap::from([(table.clone(), base_table.clone())]),
            vec![btree_index(&table, &index_name, "name")],
        );
        let empty_snapshot =
            bucket_snapshot(&bucket.app, std::collections::BTreeMap::new(), vec![]);
        compose_bucket_migration(
            &work,
            &bucket,
            v1_snapshot.clone(),
            empty_snapshot,
            "drift-index-v1",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19733),
        );
        assert_eq!(
            run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await,
            0
        );

        ctx.raw_execute(&format!("DROP INDEX {index_name}"), &[])
            .await
            .expect("drop index out of band");

        let v2_snapshot = bucket_snapshot(
            &bucket.app,
            std::collections::BTreeMap::from([(
                table.clone(),
                simple_table(
                    &bucket.app,
                    &table,
                    vec![id_col(), text_col("name", false), text_col("email", true)],
                ),
            )]),
            vec![btree_index(&table, &index_name, "name")],
        );
        compose_bucket_migration(
            &work,
            &bucket,
            v2_snapshot,
            v1_snapshot,
            "drift-index-v2",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19734),
        );

        let exit = run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await;
        assert_eq!(exit, 2, "dropped index drift must refuse");
    }

    #[djogi::djogi_test]
    async fn apply_refuses_on_foreign_key_shape_drift(mut ctx: djogi::context::DjogiContext) {
        static DRIFT_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = DRIFT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let users = format!("drift_fk_users_{n}");
        let posts = format!("drift_fk_posts_{n}");
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let work = temp_workspace("drift-fk-refusal");

        let v1_models = std::collections::BTreeMap::from([
            (
                users.clone(),
                simple_table(&bucket.app, &users, vec![id_col(), text_col("name", false)]),
            ),
            (
                posts.clone(),
                simple_table(
                    &bucket.app,
                    &posts,
                    vec![
                        id_col(),
                        bigint_fk_col("user_id", &users, djogi::migrate::OnDeleteSchema::Restrict),
                    ],
                ),
            ),
        ]);
        let v1_snapshot = bucket_snapshot(&bucket.app, v1_models.clone(), vec![]);
        let empty_snapshot =
            bucket_snapshot(&bucket.app, std::collections::BTreeMap::new(), vec![]);
        compose_bucket_migration(
            &work,
            &bucket,
            v1_snapshot.clone(),
            empty_snapshot,
            "drift-fk-v1",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19735),
        );
        assert_eq!(
            run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await,
            0
        );

        let fk_name = ctx
            .raw_scalar::<String>(
                "SELECT c.conname
                 FROM pg_constraint c
                 JOIN pg_class t ON t.oid = c.conrelid
                 WHERE t.relname = $1 AND c.contype = 'f'
                 LIMIT 1",
                &[&posts.as_str()],
            )
            .await
            .expect("lookup live FK name");
        ctx.raw_ddl(&format!(
            "ALTER TABLE {posts} DROP CONSTRAINT {fk_name}; \
             ALTER TABLE {posts} ADD CONSTRAINT {fk_name} \
             FOREIGN KEY (user_id) REFERENCES {users}(id) ON DELETE CASCADE"
        ))
        .await
        .expect("rewrite FK out of band");

        let v2_models = std::collections::BTreeMap::from([
            (
                users.clone(),
                simple_table(&bucket.app, &users, vec![id_col(), text_col("name", false)]),
            ),
            (
                posts.clone(),
                simple_table(
                    &bucket.app,
                    &posts,
                    vec![
                        id_col(),
                        bigint_fk_col("user_id", &users, djogi::migrate::OnDeleteSchema::Restrict),
                        text_col("note", true),
                    ],
                ),
            ),
        ]);
        compose_bucket_migration(
            &work,
            &bucket,
            bucket_snapshot(&bucket.app, v2_models, vec![]),
            v1_snapshot,
            "drift-fk-v2",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19736),
        );

        let exit = run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await;
        assert_eq!(exit, 2, "FK shape drift must refuse");
    }

    #[djogi::djogi_test]
    async fn fake_apply_succeeds_with_corrupt_snapshot_on_disk(
        mut ctx: djogi::context::DjogiContext,
    ) {
        static DRIFT_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = DRIFT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let table = format!("fake_corrupt_snapshot_{n}");
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let work = temp_workspace("fake-corrupt-snapshot");

        let v1_models = std::collections::BTreeMap::from([(
            table.clone(),
            simple_table(&bucket.app, &table, vec![id_col(), text_col("name", false)]),
        )]);
        let v1_snapshot = bucket_snapshot(&bucket.app, v1_models.clone(), vec![]);
        let empty_snapshot =
            bucket_snapshot(&bucket.app, std::collections::BTreeMap::new(), vec![]);
        compose_bucket_migration(
            &work,
            &bucket,
            v1_snapshot.clone(),
            empty_snapshot,
            "fake-corrupt-v1",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19737),
        );
        assert_eq!(
            run_apply_in_test_db(&mut ctx, &work, FakeMode::Real).await,
            0
        );

        let v2_models = std::collections::BTreeMap::from([(
            table.clone(),
            simple_table(
                &bucket.app,
                &table,
                vec![id_col(), text_col("name", false), text_col("email", true)],
            ),
        )]);
        let v2_snapshot = bucket_snapshot(&bucket.app, v2_models, vec![]);
        let report = compose_bucket_migration(
            &work,
            &bucket,
            v2_snapshot.clone(),
            v1_snapshot,
            "fake-corrupt-v2",
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(19738),
        );
        let second_version = report.composed_buckets[0].version.clone();

        let snap_path = reconstruct_snapshot_path(&work, &bucket);
        fs::write(&snap_path, b"not json").expect("corrupt snapshot");

        let exit = run_apply_in_test_db(
            &mut ctx,
            &work,
            FakeMode::Fake {
                reason: "schema pre-exists (corrupt-snapshot guard)".to_string(),
            },
        )
        .await;
        assert_eq!(exit, 0, "fake apply must ignore corrupt snapshot");

        let status = ctx
            .raw_scalar::<String>(
                "SELECT status FROM djogi_schema_migrations \
                 WHERE version = $1 AND app_label = $2",
                &[&second_version, &bucket.app],
            )
            .await
            .expect("query fake row status");
        assert_eq!(status, "faked");

        let repaired_snapshot = djogi::migrate::load_snapshot(&snap_path)
            .expect("fake apply should rewrite a valid snapshot");
        assert!(
            repaired_snapshot.models.contains_key(&table),
            "fake apply should persist the caller-supplied snapshot forward"
        );
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

    #[serial_test::serial]
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

    #[serial_test::serial]
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

    #[serial_test::serial]
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

    /// All operator-actionable compose refusals should map to exit code
    /// `2`; these are conditions the operator must fix and rerun.
    #[test]
    fn u4_compose_refusal_variants_map_to_exit_code_two() {
        use djogi::migrate::{BucketKey, Classification, ComposeError};

        let bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };

        let cases = [
            ComposeError::TombstonedAppRequiresAllowDestructive {
                app_label: "billing".to_string(),
                database: "main".to_string(),
                text: "D011: tombstoned app".to_string(),
            },
            ComposeError::DestructiveRequiresAllowDestructive {
                bucket: bucket.clone(),
                classification: Classification::Lossy,
            },
            ComposeError::UnsupportedDelta {
                bucket: bucket.clone(),
                reason: "unsupported transition".to_string(),
            },
            ComposeError::HandEditedMigrationWouldBeOverwritten {
                bucket: bucket.clone(),
                path: std::path::PathBuf::from("/tmp/example.sdjql"),
                text: "D013: hand-edited migration would be overwritten".to_string(),
            },
            ComposeError::PendingJsonWouldBeOverwritten {
                path: std::path::PathBuf::from("/tmp/example.pending"),
                text: "pending json mismatch".to_string(),
            },
            ComposeError::FolderRenameTargetCollision {
                from: std::path::PathBuf::from("/tmp/old"),
                to: std::path::PathBuf::from("/tmp/new"),
                offending_entry: "users.sdjql".to_string(),
            },
            ComposeError::LinkageDropWithoutModels {
                app_label: "billing".to_string(),
                database: "main".to_string(),
                text: "D010: linkage drop without models".to_string(),
            },
            ComposeError::CrossBucketForeignKeyCycle {
                database: "main".to_string(),
                chain: vec!["a".to_string(), "b".to_string()],
            },
        ];

        for case in &cases {
            assert_eq!(
                compose_error_exit_code(case),
                2,
                "compose refusal variant must map to exit 2: {case}"
            );
        }
    }

    /// `NothingToCompose` maps to exit `0` directly through the helper.
    /// The compose command intercepts it upstream and returns `0` before
    /// reaching the helper, but the helper's contract must still be total
    /// and self-consistent — returning `0`, not falling through to the
    /// `#[non_exhaustive]` wildcard's `1`.
    #[test]
    fn u4_compose_nothing_to_compose_maps_to_exit_code_zero() {
        use djogi::migrate::ComposeError;

        assert_eq!(
            compose_error_exit_code(&ComposeError::NothingToCompose),
            0,
            "NothingToCompose must map to exit 0 in the helper, not the wildcard's 1"
        );
    }

    /// Runtime, serialization, and framework-internal `ComposeError`
    /// variants remain exit code `1` so CI can treat them as retryable
    /// infra issues rather than operator-actionable refusals. Covers the
    /// constructible exit-1 shapes across every wrapper: top-level I/O and
    /// serialization, the `SqlEmit` routing-invariant violation, the
    /// `Diff` framework-internal invariant, and the `PhaseZeroAutoEmit`
    /// transient/serialization variants.
    #[test]
    fn u4_compose_runtime_variants_map_to_exit_code_one() {
        use djogi::migrate::schema::PkKindSchema;
        use djogi::migrate::{
            AutoEmitError, ComposeError, DiffError, PkFlipError, SnapshotError, SqlEmitError,
        };

        let cases = [
            // Top-level transient I/O.
            ComposeError::Io {
                path: std::path::PathBuf::from("/tmp/io-failure"),
                source: std::io::Error::other("io failure"),
            },
            // SQL emit reached by a framework-routing invariant violation
            // (a PK-type flip must route through the PK-flip playbook, not
            // the standard emitter) — not operator-caused.
            ComposeError::SqlEmit(SqlEmitError::PkTypeFlipMustRouteToT9 {
                table: "orders".to_string(),
                from: PkKindSchema::HeerIdRecencyBiased,
                to: PkKindSchema::HeerId,
            }),
            // Differential failure rooted in a framework-internal invariant
            // (self-FK sidecar metadata lengths out of sync). `PkFlipError`
            // is re-exported from `djogi::migrate` and its sole variant has
            // all-public fields, so this exit-1 `DiffError` sub-variant is
            // constructible directly here.
            ComposeError::Diff(DiffError::PkFlipMalformedSelfFkMetadata(
                PkFlipError::MalformedSelfFkMetadata {
                    parent_table: "nodes".to_string(),
                    fk_columns: 2,
                    fk_constraint_names: 1,
                    fk_deferrable: 2,
                    fk_initially_deferred: 2,
                },
            )),
            // Phase-zero bootstrap with transient I/O.
            ComposeError::PhaseZeroAutoEmit(AutoEmitError::Io {
                path: std::path::PathBuf::from("/tmp/bootstrap.sdjql"),
                source: std::io::Error::other("disk full"),
            }),
            // Phase-zero bootstrap with a serialization failure.
            ComposeError::PhaseZeroAutoEmit(AutoEmitError::PendingJson(
                serde_json::from_str::<serde_json::Value>("not-json-at-all").unwrap_err(),
            )),
            // Checksum / snapshot serialization failure.
            ComposeError::SerializeFailed(SnapshotError::Parse {
                path: None,
                source: serde_json::from_str::<serde_json::Value>("not-json").unwrap_err(),
            }),
        ];

        for case in &cases {
            assert_eq!(
                compose_error_exit_code(case),
                1,
                "compose runtime variant must map to exit 1: {case}"
            );
        }
    }

    /// Operator-actionable sub-variants nested inside the `SqlEmit`,
    /// `Diff`, and `PhaseZeroAutoEmit` wrappers must map to exit code `2`
    /// just like the flat refusals — the wrapper alone does not decide the
    /// code. The companion `u4_compose_refusal_variants_map_to_exit_code_two`
    /// covers the eight flat top-level refusals; this covers the nested set.
    #[test]
    fn u4_compose_nested_refusal_variants_map_to_exit_code_two() {
        use djogi::migrate::{
            AutoEmitError, BootstrapError, ComposeError, DiffError, SqlEmitError,
        };

        let cases = [
            // SQL emit operator-actionable sub-variants.
            ComposeError::SqlEmit(SqlEmitError::Unsupported {
                reason: "enum-variant removal requires hand-written migration".to_string(),
            }),
            ComposeError::SqlEmit(SqlEmitError::UnsupportedPartitionChange {
                table: "events".to_string(),
                detail: "changing partition method requires full table rebuild".to_string(),
            }),
            ComposeError::SqlEmit(SqlEmitError::InvalidStorageParams {
                params: "fillfactor=invalid".to_string(),
                reason: "fillfactor must be an integer 10..100".to_string(),
            }),
            // Differential operator-actionable sub-variants.
            ComposeError::Diff(DiffError::PkFlipCascadeDepthExceeded {
                parent_table: "vehicles".to_string(),
                chain: vec!["vehicles".to_string(), "parts".to_string()],
                max_depth: 65,
            }),
            ComposeError::Diff(DiffError::PartitionedMultiParentClusterUnsupported {
                partitioned_parents: vec!["invoices".to_string()],
                cross_flipping_partners: vec!["invoices".to_string(), "line_items".to_string()],
            }),
            // Phase-zero bootstrap extension refusals.
            ComposeError::PhaseZeroAutoEmit(AutoEmitError::Compose(
                BootstrapError::InvalidExtensionName {
                    name: "bad-name!".to_string(),
                },
            )),
            ComposeError::PhaseZeroAutoEmit(AutoEmitError::Compose(
                BootstrapError::UnknownExtension {
                    name: "not_in_allowlist".to_string(),
                },
            )),
            // A `DiffError` forwarded through the SQL emitter layer is
            // classified identically to a top-level `Diff`.
            ComposeError::SqlEmit(SqlEmitError::Diff(DiffError::PkFlipCascadeDepthExceeded {
                parent_table: "widgets".to_string(),
                chain: vec!["widgets".to_string()],
                max_depth: 65,
            })),
        ];

        for case in &cases {
            assert_eq!(
                compose_error_exit_code(case),
                2,
                "nested operator-actionable compose refusal must map to exit 2: {case}"
            );
        }
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

    // ── issue #355: rollback CLI exit / guard mapping ───────────────────

    #[test]
    fn rollback_lossy_opt_in_requires_non_empty_reason() {
        let workspace = Some(std::path::PathBuf::from("/tmp/nonexistent_djogi_ws"));

        let missing = rollback_cmd(
            None,
            false,
            true,
            None,
            None,
            None,
            workspace.clone(),
            None,
            false,
        );
        assert_eq!(
            missing,
            ExitCode::from(2),
            "--allow-data-loss without --reason must exit 2 before any DB work"
        );

        let blank = rollback_cmd(
            None,
            false,
            true,
            Some("   ".to_string()),
            None,
            None,
            workspace,
            None,
            false,
        );
        assert_eq!(
            blank,
            ExitCode::from(2),
            "blank --reason with --allow-data-loss must exit 2 before any DB work"
        );
    }

    #[serial_test::serial]
    #[test]
    fn rollback_allow_data_loss_with_whitespace_reason_exits_code_2() {
        let work = temp_workspace("rollback-empty-reason");
        write_unreachable_config(&work);
        let exit = without_database_url(|| {
            rollback_cmd(
                None,
                false,
                true,
                Some("   ".to_string()),
                None,
                None,
                Some(work.clone()),
                None,
                true,
            )
        });
        assert_eq!(exit, ExitCode::from(2));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[serial_test::serial]
    #[test]
    fn rollback_missing_identity_exits_code_2_before_db_work() {
        let work = temp_workspace("rollback-no-identity");
        write_unreachable_config(&work);
        let exit = without_database_url(|| {
            rollback_cmd(
                None,
                false,
                false,
                None,
                None,
                None,
                Some(work.clone()),
                None,
                false,
            )
        });
        assert_eq!(exit, ExitCode::from(2));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[serial_test::serial]
    #[test]
    fn rollback_unreachable_database_exits_code_1() {
        let work = temp_workspace("rollback-unreachable");
        write_unreachable_config(&work);
        let exit = without_database_url(|| {
            rollback_cmd(
                None,
                false,
                false,
                None,
                None,
                None,
                Some(work.clone()),
                None,
                true,
            )
        });
        assert_eq!(exit, ExitCode::from(1));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn rollback_refusal_variants_map_to_exit_code_two() {
        use djogi::migrate::LossyRollbackKind;

        let cases = [
            RollbackError::LossyRollbackRefused {
                offending_labels: vec!["DropTable widgets".to_string()],
                kinds: vec![LossyRollbackKind::DropTable],
            },
            RollbackError::VersionNotRollbackable {
                version: "V20260101000000__widgets".to_string(),
                current_status: LedgerStatus::Pending,
            },
            RollbackError::VersionNotFound {
                version: "V20260101000000__widgets".to_string(),
            },
            RollbackError::MissingRollbackIdentity {
                version: "V20260101000000__widgets".to_string(),
            },
        ];

        for err in &cases {
            assert_eq!(
                rollback_error_exit_code(err),
                2,
                "rollback refusal variant must map to exit 2: {err}"
            );
        }
    }

    #[test]
    fn rollback_runner_refusal_variants_map_to_exit_code_two() {
        let err = RollbackError::Runner {
            source: RunnerError::PartitionExpansionNoLeaves {
                parent: "public.events".to_string(),
                statement_label: "expand_partitions".to_string(),
            },
            live_db_committed: false,
        };
        assert_eq!(
            rollback_error_exit_code(&err),
            2,
            "rollback should classify replay-strict refusal variants as exit 2"
        );
    }

    #[test]
    fn rollback_transient_variants_map_to_exit_code_one() {
        use djogi::error::{DbError, DjogiError};

        let cases = [
            RollbackError::Runner {
                source: RunnerError::PinnedSessionCheckoutFailed {
                    source: DjogiError::Db(DbError::other("pool exhausted")),
                },
                live_db_committed: false,
            },
            RollbackError::DownStatementFailed {
                segment_index: 0,
                statement_label: "DROP TABLE widgets".to_string(),
                live_db_committed: false,
                source: DjogiError::Db(DbError::other("syntax error")),
            },
        ];

        for err in &cases {
            assert_eq!(
                rollback_error_exit_code(err),
                1,
                "rollback transient variant must map to exit 1: {err}"
            );
        }
    }

    #[test]
    fn rollback_snapshot_persist_failed_maps_to_exit_two() {
        use djogi::migrate::SnapshotError;

        // SnapshotPersistFailed only fires after the rollback SQL committed,
        // so it is an operator-actionable repair signal (rebuild the snapshot),
        // not a transient failure a blind retry could clear. It must map to
        // exit 2 like the other refusals, not exit 1.
        let err = RollbackError::SnapshotPersistFailed {
            path: PathBuf::from("/tmp/schema_snapshot.json"),
            source: SnapshotError::Io {
                path: Some(PathBuf::from("/tmp/schema_snapshot.json")),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            },
        };
        assert_eq!(
            rollback_error_exit_code(&err),
            2,
            "SnapshotPersistFailed must map to exit 2 (post-commit repair signal)"
        );
    }

    #[test]
    fn rollback_targets_without_to_selects_newest_applied_row_only() {
        let rows = vec![
            ledger_row(10, "V20260101000001__old", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__new", LedgerStatus::Applied, ""),
            ledger_row(
                12,
                "V20260101000003__other_app",
                LedgerStatus::Applied,
                "billing",
            ),
        ];

        let selected = select_rollback_targets(&rows, "", None).expect("selection ok");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].version, "V20260101000002__new");
    }

    #[test]
    fn rollback_targets_with_to_selects_every_newer_non_rolled_back_row() {
        let rows = vec![
            ledger_row(10, "V20260101000001__base", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__middle", LedgerStatus::Faked, ""),
            ledger_row(12, "V20260101000003__newest", LedgerStatus::Applied, ""),
        ];

        let selected = select_rollback_targets(&rows, "", Some("V20260101000001__base"))
            .expect("selection ok");
        assert_eq!(
            selected
                .iter()
                .map(|row| row.version.as_str())
                .collect::<Vec<_>>(),
            vec!["V20260101000003__newest", "V20260101000002__middle"]
        );
    }

    #[test]
    fn rollback_targets_refuse_pending_row_inside_to_range() {
        let rows = vec![
            ledger_row(10, "V20260101000001__base", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__pending", LedgerStatus::Pending, ""),
            ledger_row(12, "V20260101000003__newest", LedgerStatus::Applied, ""),
        ];

        let err = select_rollback_targets(&rows, "", Some("V20260101000001__base"))
            .expect_err("pending row must refuse");
        assert!(err.contains("resolve it with `djogi migrations repair`"));
        assert!(err.contains("V20260101000002__pending"));
    }

    #[test]
    fn rollback_targets_refuse_missing_to_version() {
        let rows = vec![ledger_row(
            10,
            "V20260101000001__base",
            LedgerStatus::Applied,
            "",
        )];

        let err = select_rollback_targets(&rows, "", Some("V20260101000099__missing"))
            .expect_err("missing --to must refuse");
        assert!(err.contains("is not present in this bucket's ledger"));
    }

    #[test]
    fn rollback_targets_skip_rolled_back_rows_inside_to_range() {
        // A previously rolled-back row interleaved with applied rows must be
        // skipped (continue), not break the walk or be re-selected.
        let rows = vec![
            ledger_row(10, "V20260101000001__base", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__undone", LedgerStatus::RolledBack, ""),
            ledger_row(12, "V20260101000003__newest", LedgerStatus::Applied, ""),
        ];

        let selected = select_rollback_targets(&rows, "", Some("V20260101000001__base"))
            .expect("selection ok");
        assert_eq!(
            selected
                .iter()
                .map(|row| row.version.as_str())
                .collect::<Vec<_>>(),
            // The rolled-back middle row is absent; only the still-applied
            // newest row above the floor is selected.
            vec!["V20260101000003__newest"]
        );
    }

    #[test]
    fn rollback_targets_without_to_stop_at_newest_baseline_row() {
        // With no `--to`, encountering a baseline row as the newest entry
        // breaks the walk and returns an empty set — nothing newer to undo.
        let rows = vec![
            ledger_row(10, "V20260101000001__older", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__baseline", LedgerStatus::Baseline, ""),
        ];

        let selected = select_rollback_targets(&rows, "", None).expect("selection ok");
        assert!(
            selected.is_empty(),
            "baseline as newest row must yield no rollback targets, got {selected:?}"
        );
    }

    #[test]
    fn rollback_targets_refuse_baseline_above_floor() {
        // With `--to` pointing at an older applied row, a baseline row sitting
        // ABOVE the floor cannot be rolled back past — refuse explicitly.
        let rows = vec![
            ledger_row(10, "V20260101000001__base", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__baseline", LedgerStatus::Baseline, ""),
            ledger_row(12, "V20260101000003__newest", LedgerStatus::Applied, ""),
        ];

        let err = select_rollback_targets(&rows, "", Some("V20260101000001__base"))
            .expect_err("baseline above floor must refuse");
        assert!(err.contains("cannot roll back past baseline"));
        assert!(err.contains("V20260101000002__baseline"));
    }

    #[test]
    fn rollback_targets_refuse_failed_row_inside_to_range() {
        // A `failed` row in the rollback range must refuse with the same
        // repair pointer as a pending row.
        let rows = vec![
            ledger_row(10, "V20260101000001__base", LedgerStatus::Applied, ""),
            ledger_row(11, "V20260101000002__failed", LedgerStatus::Failed, ""),
            ledger_row(12, "V20260101000003__newest", LedgerStatus::Applied, ""),
        ];

        let err = select_rollback_targets(&rows, "", Some("V20260101000001__base"))
            .expect_err("failed row must refuse");
        assert!(err.contains("resolve it with `djogi migrations repair`"));
        assert!(err.contains("V20260101000002__failed"));
    }

    #[test]
    fn rollback_drift_identical_sets_pass() {
        let rows = [ledger_row(1, "V1__a", LedgerStatus::Applied, "")];
        let pre: Vec<_> = rows.iter().collect();
        let locked: Vec<_> = rows.iter().collect();
        assert!(ensure_no_target_drift(&pre, &locked).is_ok());
    }

    #[test]
    fn rollback_drift_between_reads_refuses_with_rerun() {
        let pre_rows = [ledger_row(1, "V1__a", LedgerStatus::Applied, "")];
        let locked_rows = [
            ledger_row(1, "V1__a", LedgerStatus::Applied, ""),
            ledger_row(2, "V2__b", LedgerStatus::Applied, ""),
        ];
        let pre: Vec<_> = pre_rows.iter().collect();
        let locked: Vec<_> = locked_rows.iter().collect();
        let err = ensure_no_target_drift(&pre, &locked).expect_err("drift must refuse");
        assert!(err.contains("rerun"));
    }

    #[test]
    fn lossy_scan_detects_both_marker_spellings() {
        let down = "-- Djogi composed migration — down\n\
                    -- LOSSY: DroppedColumn — data in `horsepower` is lost\n\
                    ALTER TABLE vehicles DROP COLUMN horsepower;\n\
                    -- LOSSY ROLLBACK: cannot recreate table `probe` from the diff.\n";
        let hits = scan_lossy_down_markers(down);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].starts_with("-- LOSSY:"));
        assert!(hits[1].starts_with("-- LOSSY ROLLBACK:"));
    }

    #[test]
    fn lossy_scan_ignores_plain_comments_and_sql() {
        let down = "-- ordinary comment\nDROP TABLE probe;\n";
        assert!(scan_lossy_down_markers(down).is_empty());
    }

    #[test]
    fn non_transactional_down_shape_is_detectable_via_public_api() {
        assert_eq!(
            djogi::migrate::find_non_transactional_statement_shape(
                "CREATE INDEX CONCURRENTLY idx_probe ON widgets (id);"
            ),
            Some("CREATE INDEX CONCURRENTLY"),
        );
        assert_eq!(
            djogi::migrate::find_non_transactional_statement_shape("DROP TABLE widgets;"),
            None,
        );
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
    /// non-terminal), a wiring bug that supplies a snapshot, a
    /// post-applied-version snapshot persistence failure, and an
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
            RunnerError::SnapshotPersistFailed {
                path: std::path::PathBuf::from("/tmp/schema_snapshot.json"),
                source: djogi::migrate::snapshot::SnapshotError::Io {
                    path: None,
                    source: std::io::Error::other("disk full"),
                },
            },
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
                runner_error_exit_code(err),
                2,
                "baseline refusal variant must map to exit 2: {err}"
            );
        }
    }

    /// Transient `RunnerError` variants reachable from the baseline path
    /// must map to exit `1` (retryable). Classification is exhaustive in
    /// the library (`RunnerError::is_operator_actionable`) — these
    /// representative cases pin the projection / ledger / snapshot
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
                runner_error_exit_code(err),
                1,
                "baseline transient variant must map to exit 1: {err}"
            );
        }
    }

    /// Runner errors mapped through `migrations apply` retain the
    /// library's operator-actionability semantics: refusals are exit `2`
    /// and retryable transient failures stay at exit `1`.
    #[test]
    fn apply_runner_refusal_variants_map_to_exit_code_two() {
        let cases = [
            RunnerError::VersionAlreadyApplied {
                version: "V20260101000000__add_users".to_string(),
                applied_at: None,
            },
            RunnerError::VersionCollisionNonTerminal {
                version: "V20260101000000__add_users".to_string(),
                status: LedgerStatus::Pending,
                run_id: 1,
            },
            RunnerError::StalePhaseZeroArtifact {
                version: "V00000000000000__phase_zero".to_string(),
                refusal_reason: "generated-stale",
            },
            RunnerError::OutOfOrderRejected {
                version: "V20260101000000__add_users".to_string(),
                conflicting_version: "V20260201000000__add_more".to_string(),
                conflicting_applied_at: Some("2026-01-01T00:00:00Z".to_string()),
            },
            RunnerError::SnapshotPersistFailed {
                path: std::path::PathBuf::from("/tmp/schema_snapshot.json"),
                source: djogi::migrate::snapshot::SnapshotError::Io {
                    path: None,
                    source: std::io::Error::other("disk full"),
                },
            },
            RunnerError::PkFlipHazardDisabledTriggers {
                table: "public.events".to_string(),
                triggers: vec![("zzz_rv_events_id".to_string(), 'D')],
            },
            RunnerError::PartitionExpansionNoLeaves {
                parent: "public.events".to_string(),
                statement_label: "expand_partitions".to_string(),
            },
        ];
        for err in &cases {
            assert_eq!(
                runner_error_exit_code(err),
                2,
                "runner refusal variant must map to exit 2: {err}"
            );
        }
    }

    #[test]
    fn apply_runner_drift_variants_map_to_expected_exit_codes() {
        use djogi::error::{DbError, DjogiError};

        let bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let report = VerifyReport {
            diagnostics: vec![djogi::migrate::VerifyDiagnostic {
                code: "D601".to_string(),
                severity: VerifySeverity::Error,
                message: "Snapshot table missing from live DB".to_string(),
                location: Some("billing.invoices".to_string()),
            }],
            latest_applied_version: Some("V20260601000000__billing".to_string()),
            applied_count: 2,
            unfinished_count: 0,
        };
        let cases = [
            (
                RunnerError::DriftDetected {
                    bucket: bucket.clone(),
                    report,
                },
                2,
            ),
            (
                RunnerError::DriftBaselineMissing {
                    bucket: bucket.clone(),
                },
                2,
            ),
            (
                RunnerError::DriftBaselineCorrupted {
                    bucket: bucket.clone(),
                    reason: "unexpected end of input".to_string(),
                },
                2,
            ),
            (
                RunnerError::DriftPreflightFailed {
                    source: Box::new(djogi::migrate::verify::VerifyRunError::CatalogQueryFailed {
                        query_label: "columns",
                        source: DjogiError::Db(DbError::other("catalog read failed")),
                    }),
                },
                1,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(
                runner_error_exit_code(&err),
                expected,
                "drift variant must map to exit code {expected}: {err}"
            );
        }
    }

    /// Runner errors surfaced through `migrations apply` remain retryable
    /// where a blind retry can still make progress after transient
    /// recovery.
    #[test]
    fn apply_runner_transient_variants_map_to_exit_code_one() {
        use djogi::error::{DbError, DjogiError};
        let cases = [
            RunnerError::LockTimeout {
                path: std::path::PathBuf::from("/tmp/.djogi-migrations-lock"),
                holder_pid: None,
            },
            RunnerError::LedgerWriteFailed {
                version: "V20260101000000__add_users".to_string(),
                source: DjogiError::Db(DbError::other("insert failed")),
            },
            RunnerError::CatalogQueryFailed {
                query_label: "pg_class relpages",
                source: DjogiError::Db(DbError::other("query failed")),
            },
            RunnerError::PinnedSessionCheckoutFailed {
                source: DjogiError::Db(DbError::other("pool unavailable")),
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
                runner_error_exit_code(err),
                1,
                "runner transient variant must map to exit 1: {err}"
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

    #[test]
    fn load_drift_baseline_fake_skips_filesystem_and_real_missing_maps_missing() {
        let work = temp_workspace("load-drift-baseline");
        let missing = work.join("schema_snapshot.json");

        assert!(
            matches!(
                load_drift_baseline(
                    &FakeMode::Fake {
                        reason: "adopt existing schema".to_string(),
                    },
                    &missing,
                ),
                DriftBaseline::Disabled
            ),
            "fake apply must not touch the snapshot path"
        );
        assert!(
            matches!(
                load_drift_baseline(&FakeMode::Real, &missing),
                DriftBaseline::Missing
            ),
            "real apply must surface missing snapshot as a typed baseline state"
        );
    }

    #[test]
    fn load_drift_baseline_real_corrupt_snapshot_maps_to_corrupted() {
        let work = temp_workspace("load-drift-baseline-corrupt");
        let path = work.join("schema_snapshot.json");
        fs::write(&path, b"{ not json").unwrap();
        let baseline = load_drift_baseline(&FakeMode::Real, &path);
        assert!(
            matches!(baseline, DriftBaseline::Corrupted(_)),
            "corrupt snapshot must map to DriftBaseline::Corrupted, got: {baseline:?}"
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

    #[test]
    fn render_drift_refusal_appends_next_steps_trailer() {
        use djogi::migrate::{VerifyReport, VerifySeverity};

        let report = VerifyReport {
            diagnostics: vec![diag(
                "D601",
                VerifySeverity::Error,
                "Snapshot table missing from live DB",
                Some("billing.invoices"),
            )],
            latest_applied_version: Some("V20260601000000__billing".to_string()),
            applied_count: 2,
            unfinished_count: 0,
        };
        let lines = render_drift_refusal(&render_bucket("main", "billing"), &report);

        assert!(
            lines
                .iter()
                .any(|line| line.contains("Apply refused before any migration SQL ran")),
            "missing refusal trailer: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("djogi migrations verify")),
            "missing verify guidance: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("djogi migrations attune")),
            "missing attune guidance: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("repair resume-partial")),
            "missing resume-partial guidance: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("repair snapshot-rebuild")),
            "DriftDetected trailer must not mention snapshot-rebuild (that is for DriftBaselineMissing): {lines:?}"
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
