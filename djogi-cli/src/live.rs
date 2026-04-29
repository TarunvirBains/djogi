//! `djogi live` — operator surface for Phase 7.5 live migrations.
//!
//! Six subcommands materialise the operator-facing contract for the
//! expand → backfill → flip → contract sequence the live-plan layer
//! drives:
//!
//! - `live plan` — generate plan files for pending schema deltas
//!   classified [`OnlineSafetyClassification::ExpandContract`](djogi::live_migrate::OnlineSafetyClassification::ExpandContract).
//! - `live show` — render plan-file metadata + persisted runtime state
//!   side by side, plus the active hook snapshot at the current step.
//! - `live run` — drive the plan forward until the next operator gate,
//!   refusing destructive work without `--allow-destructive`
//!   `--justify "<reason>"`.
//! - `live resume` — pick up after an interruption; reads
//!   `backfill_rows_done` from the row and continues at the same step.
//! - `live finalize` — execute remaining
//!   [`StepKind::FinalizeConstraints`](djogi::live_migrate::StepKind::FinalizeConstraints)
//!   /
//!   [`StepKind::CleanupLegacyState`](djogi::live_migrate::StepKind::CleanupLegacyState)
//!   steps, drop compatibility hooks, and promote the row to
//!   [`PlanStatus::Complete`](djogi::live_migrate::PlanStatus::Complete).
//! - `live abandon` — terminal opt-out; gated on confirmation OR
//!   `--force` plus a non-production `DJOGI_ENV`.
//!
//! # Exit codes
//!
//! Per v3 §3 of the Phase 7.5 plan:
//!
//! | Code | Meaning |
//! |------|---------|
//! | 0 | Success — operator may invoke the next subcommand. |
//! | 1 | Step failed — generic runtime error (config / network / SQL / I/O). |
//! | 2 | Classification refused to plan — the delta is `OfflineOnly`. |
//! | 3 | Validation checkpoint failed — gate query disagreed. |
//! | 4 | Plan-file checksum drift — the file was edited after start. |
//! | 5 | Plan state conflicts with request (e.g. `run` on `complete`). |
//!
//! # Out of scope for T10
//!
//! - **Live-DB integration tests.** T12 owns end-to-end coverage; T10
//!   ships clap parsing, helpers, and exit-code mapping.
//! - **`djogi_codec_recode(...)`** — still a placeholder per T8.
//! - **`djogi_schema_migrations.justification` persistence.** Adding
//!   the column requires a separate ALTER TABLE migration; T10 accepts
//!   `--justify` and routes the value through to the runner, but the
//!   actual column write lands in a follow-up phase.

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use clap::Subcommand;
use djogi::config::DjogiConfig;
use djogi::context::DjogiContext;
use djogi::live_migrate::{
    LivePlanRow, PlanFileError, PlanStatus, active_hooks_at_step, plan_path, read_plan,
    verify_checksum,
};
use djogi::pg::pool::DjogiPool;
use djogi::types::HeerId;

// ── Subcommand surface ────────────────────────────────────────────────

/// Operator surface for the live-migration runner. Every subcommand
/// resolves to an [`ExitCode`] via [`dispatch`].
#[derive(Debug, Subcommand)]
pub enum LiveCmd {
    /// Generate plan file(s) for pending schema deltas classified
    /// `ExpandContract`. Refuses (exit 2) when the delta is
    /// `OfflineOnly`; reports "no live plan needed" (exit 0) when the
    /// delta is fully `OnlineSafe` and Phase 7's regular runner can
    /// handle it directly.
    Plan {
        /// Optional explicit migration version to materialize.
        version: Option<String>,
        /// Workspace root override. Defaults to the current working
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Show plan-file metadata, runtime state, and active hooks at the
    /// current step.
    Show {
        /// HeerId rendered as decimal — same shape the plan file's
        /// filename embeds.
        plan_id: String,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Drive the plan forward until the next operator checkpoint
    /// (validate / cutover / finalize). Returns 0 at every checkpoint
    /// so the operator script can branch cleanly.
    Run {
        plan_id: String,
        /// Allow destructive steps to execute. Without this flag the
        /// runner refuses any
        /// [`StepKind::CleanupLegacyState`](djogi::live_migrate::StepKind::CleanupLegacyState)
        /// step with exit code 1.
        #[arg(long, default_value_t = false)]
        allow_destructive: bool,
        /// Operator-supplied justification recorded alongside any
        /// destructive step. Required when `--allow-destructive` is
        /// set, and required when `--allow-raw-dangerous` is set.
        #[arg(long)]
        justify: Option<String>,
        /// Opt-in to running plans whose source migration was
        /// hand-edited (per v3 §8 amendment). Always paired with
        /// `--justify`.
        #[arg(long, default_value_t = false)]
        allow_raw_dangerous: bool,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Continue after an interruption. Reads `backfill_rows_done`
    /// from the row and resumes at the same step.
    Resume {
        plan_id: String,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Complete cleanup + drop compatibility hooks. Gated on the row
    /// being in [`PlanStatus::Finalizing`].
    Finalize {
        plan_id: String,
        /// Operator justification recorded if any executed step is
        /// destructive (e.g. dropping the legacy column).
        #[arg(long)]
        justify: Option<String>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Mark a plan abandoned. Schema state stays at the last completed
    /// step; resuming an abandoned plan is refused. Confirmation
    /// required; `--force` requires `DJOGI_ENV` to be unset or set to
    /// any value other than `production`.
    Abandon {
        plan_id: String,
        /// Skip the interactive confirmation prompt. Requires
        /// `DJOGI_ENV != production`; refuses otherwise.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

// ── Errors ────────────────────────────────────────────────────────────

/// Errors raised by the `live` subcommand. Each variant carries a
/// dedicated exit-code mapping; conversion lives in
/// [`LiveCmdError::exit_code`].
///
/// `#[non_exhaustive]` so future failure modes (e.g. a daemon-mode
/// claim conflict) can land without breaking downstream matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LiveCmdError {
    /// Configuration / I/O / database error. Maps to exit code 1.
    #[error("{0}")]
    Runtime(String),

    /// Classification refused to plan — the delta is `OfflineOnly`.
    /// Maps to exit code 2.
    #[error("classification refused: {0}")]
    ClassificationRefused(String),

    /// A validation gate failed — the gate query returned an
    /// unexpected count / shape. Maps to exit code 3.
    #[error("validation checkpoint failed: {0}")]
    ValidationFailed(String),

    /// Plan-file checksum drift detected. Maps to exit code 4.
    #[error("plan file checksum drift: {0}")]
    ChecksumDrift(String),

    /// Plan state conflicts with the requested operation (e.g. `run`
    /// against a `complete` plan). Maps to exit code 5.
    #[error("plan state conflict: {0}")]
    StateConflict(String),

    /// Argument or gate validation failed before any side effect
    /// (missing `--justify`, `DJOGI_ENV=production` blocked `--force`,
    /// …). Maps to exit code 1 — surfaced as a runtime refusal rather
    /// than the clap default `2` so scripts can distinguish the two.
    #[error("argument refused: {0}")]
    ArgRefused(String),

    /// The plan file's HeerId could not be parsed.
    #[error("malformed plan_id: {0}")]
    MalformedPlanId(String),

    /// The plan row could not be located in `djogi_live_plans`.
    #[error("plan {0} not found in djogi_live_plans")]
    PlanNotFound(HeerId),
}

impl LiveCmdError {
    /// Map the error to the v3 §3 inline-decision exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            LiveCmdError::Runtime(_)
            | LiveCmdError::ArgRefused(_)
            | LiveCmdError::MalformedPlanId(_)
            | LiveCmdError::PlanNotFound(_) => 1,
            LiveCmdError::ClassificationRefused(_) => 2,
            LiveCmdError::ValidationFailed(_) => 3,
            LiveCmdError::ChecksumDrift(_) => 4,
            LiveCmdError::StateConflict(_) => 5,
        }
    }
}

impl From<PlanFileError> for LiveCmdError {
    fn from(value: PlanFileError) -> Self {
        match value {
            PlanFileError::ChecksumMismatch { .. } => {
                LiveCmdError::ChecksumDrift(value.to_string())
            }
            other => LiveCmdError::Runtime(other.to_string()),
        }
    }
}

// ── Top-level dispatch ────────────────────────────────────────────────

/// Top-level entry point invoked by [`crate::main`]. Builds a runtime,
/// drives the async dispatch, and folds the resulting [`LiveCmdError`]
/// to an [`ExitCode`] via [`LiveCmdError::exit_code`].
pub fn dispatch(cmd: LiveCmd) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("djogi live: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async { run(cmd).await });
    let code = match exit {
        Ok(c) => c,
        Err(e) => {
            eprintln!("djogi live: {e}");
            e.exit_code()
        }
    };
    ExitCode::from(code as u8)
}

/// Async dispatch over [`LiveCmd`]. Returns either a success exit code
/// (`0`) or a typed error whose [`LiveCmdError::exit_code`] gives the
/// non-zero return.
async fn run(cmd: LiveCmd) -> Result<i32, LiveCmdError> {
    match cmd {
        LiveCmd::Plan { version, workspace } => plan_cmd(version.as_deref(), workspace).await,
        LiveCmd::Show { plan_id, workspace } => show_cmd(&plan_id, workspace).await,
        LiveCmd::Run {
            plan_id,
            allow_destructive,
            justify,
            allow_raw_dangerous,
            workspace,
        } => {
            run_cmd(
                &plan_id,
                allow_destructive,
                justify.as_deref(),
                allow_raw_dangerous,
                workspace,
            )
            .await
        }
        LiveCmd::Resume { plan_id, workspace } => resume_cmd(&plan_id, workspace).await,
        LiveCmd::Finalize {
            plan_id,
            justify,
            workspace,
        } => finalize_cmd(&plan_id, justify.as_deref(), workspace).await,
        LiveCmd::Abandon {
            plan_id,
            force,
            workspace,
        } => abandon_cmd(&plan_id, force, workspace).await,
    }
}

// ── Argument helpers ──────────────────────────────────────────────────

/// Parse an operator-supplied `plan_id` decimal string into a
/// [`HeerId`]. Returns [`LiveCmdError::MalformedPlanId`] on any
/// parse / validation failure so the operator sees an actionable
/// message instead of a panic.
fn parse_plan_id(raw: &str) -> Result<HeerId, LiveCmdError> {
    HeerId::from_str(raw).map_err(|e| LiveCmdError::MalformedPlanId(format!("`{raw}`: {e}")))
}

/// Resolve the workspace root from the `--workspace` flag, falling
/// back to the current working directory. Mirrors the helper in
/// [`crate::migrations`] / [`crate::db`].
fn resolve_workspace(workspace: Option<PathBuf>) -> PathBuf {
    workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Reject a `live run --allow-raw-dangerous` invocation that lacks a
/// `--justify "<reason>"` value. The pairing rule comes from v3 §8
/// (the amendment that introduced `--allow-raw-dangerous`).
fn require_justify_for_dangerous(
    allow_raw_dangerous: bool,
    justify: Option<&str>,
) -> Result<(), LiveCmdError> {
    if allow_raw_dangerous && justify_is_empty(justify) {
        return Err(LiveCmdError::ArgRefused(
            "--allow-raw-dangerous requires --justify \"<reason>\"".to_string(),
        ));
    }
    Ok(())
}

/// Reject an `--allow-destructive` invocation that lacks `--justify`.
/// Operators must record why they're routing through the destructive
/// path; the reason flows into `djogi_schema_migrations.justification`
/// when that column lands (T11.x or later).
fn require_justify_for_destructive(
    allow_destructive: bool,
    justify: Option<&str>,
) -> Result<(), LiveCmdError> {
    if allow_destructive && justify_is_empty(justify) {
        return Err(LiveCmdError::ArgRefused(
            "--allow-destructive requires --justify \"<reason>\"".to_string(),
        ));
    }
    Ok(())
}

/// Pure helper: `true` when `justify` is `None` or the empty / all-
/// whitespace string. Lifted into a free function so the arg-refusal
/// guards stay symmetric.
fn justify_is_empty(justify: Option<&str>) -> bool {
    justify.map(|s| s.trim().is_empty()).unwrap_or(true)
}

/// Decide whether `live abandon --force` is permitted in the current
/// environment. Refuses when `DJOGI_ENV == "production"` (case-
/// insensitive). All other values — including unset — pass.
fn force_allowed_in_env() -> bool {
    match std::env::var("DJOGI_ENV") {
        Ok(v) => !v.eq_ignore_ascii_case("production"),
        Err(_) => true,
    }
}

// ── Plan-file location helpers ────────────────────────────────────────

/// Build the plan-file path the way [`djogi::live_migrate::plan_path`]
/// does — `<workspace>/migrations/<target_database>/live/<plan_id>_<slug>.json`.
/// Lifted into a free function so the show / run / resume / finalize
/// commands all share one path resolution.
fn resolve_plan_file_path(workspace: &std::path::Path, row: &LivePlanRow) -> std::path::PathBuf {
    let migrations_root = djogi::migrate::migrations_root(workspace);
    plan_path(
        &migrations_root,
        &row.target_database,
        row.plan_id,
        &row.slug,
    )
}

/// Best-effort connection helper. Encapsulates the repeating
/// `DjogiPool::connect` + `DjogiContext::from_pool` dance so each
/// command body stays linear.
async fn connect(database_url: &str) -> Result<DjogiContext, LiveCmdError> {
    let pool = DjogiPool::connect(database_url)
        .await
        .map_err(|e| LiveCmdError::Runtime(format!("connect: {e}")))?;
    Ok(DjogiContext::from_pool(pool))
}

/// Load `Djogi.toml` from the resolved workspace.
fn load_config(workspace: &std::path::Path) -> Result<DjogiConfig, LiveCmdError> {
    DjogiConfig::load_from_workspace(workspace)
        .map_err(|e| LiveCmdError::Runtime(format!("config load: {e}")))
}

/// Locate the row for `plan_id` in `djogi_live_plans`. Walks every
/// `(target_database, app_label)` bucket the row could live in by
/// querying directly on the primary key.
async fn fetch_row(ctx: &mut DjogiContext, plan_id: HeerId) -> Result<LivePlanRow, LiveCmdError> {
    // The bucket-keyed `fetch_row_by_id` requires the bucket fields up
    // front; the CLI only has plan_id at this entry point. We look up
    // by primary key first via raw_query (the table's PRIMARY KEY is
    // plan_id), then drop into the bucketed helper for the parsed
    // shape.
    use djogi::live_migrate::state;
    // A raw lookup against the unique PRIMARY KEY: we don't have
    // bucket fields yet, so issue a single SQL probe to read them.
    let bucket_row = ctx
        .raw_rows(
            "SELECT target_database, app_label FROM djogi_live_plans WHERE plan_id = $1",
            &[&plan_id.as_i64()],
        )
        .await
        .map_err(|e| LiveCmdError::Runtime(format!("plan lookup: {e}")))?;
    let bucket = match bucket_row.first() {
        Some(row) => {
            let target_database: String = row
                .try_get(0)
                .map_err(|e| LiveCmdError::Runtime(format!("plan lookup decode: {e}")))?;
            let app_label: String = row
                .try_get(1)
                .map_err(|e| LiveCmdError::Runtime(format!("plan lookup decode: {e}")))?;
            (target_database, app_label)
        }
        None => return Err(LiveCmdError::PlanNotFound(plan_id)),
    };
    let row = state::fetch_row_by_id(ctx, plan_id, &bucket.0, &bucket.1)
        .await
        .map_err(|e| LiveCmdError::Runtime(format!("plan fetch: {e}")))?
        .ok_or(LiveCmdError::PlanNotFound(plan_id))?;
    Ok(row)
}

// ── live plan ─────────────────────────────────────────────────────────

/// `djogi live plan` body.
///
/// Builds the descriptor projection, diffs against the persisted
/// snapshots, and routes every `ExpandContract`-classified operation
/// through [`djogi::live_migrate::dispatch_pattern`]. Emits a plan
/// file plus a `djogi_live_plans` row per bucket that needed one.
///
/// Refuses with exit code 2 when the delta carries any `OfflineOnly`
/// classification — the operator must hand-edit those rather than
/// route through `live`. Reports "no live plan needed" with exit code
/// 0 when every classification is `OnlineSafe` (Phase 7's regular
/// runner handles them directly).
async fn plan_cmd(version: Option<&str>, workspace: Option<PathBuf>) -> Result<i32, LiveCmdError> {
    let workspace = resolve_workspace(workspace);
    let _config = load_config(&workspace)?;

    // Plan file generation requires the descriptor projection +
    // snapshot diff + classifier + dispatch — a full pipeline whose
    // public entry point (`djogi::live_migrate::compose_live_plans`)
    // lands in a later task. This dispatch ships the operator-facing
    // CLI shape; until the compose-side glue lands, the body refuses
    // with an actionable message instead of half-running and leaving
    // plan files behind.
    //
    // When the engine lands, the body becomes:
    //
    //     1. project_from_inventory + load_snapshot per bucket
    //     2. diff_bucket_maps → Vec<SchemaDelta>
    //     3. classify_delta per bucket; route OfflineOnly through
    //        [`refuse_offline_only`] (exit 2)
    //     4. for each ExpandContract op: dispatch_pattern → Vec<Step>
    //     5. write_plan + insert_row per bucket
    //     6. print summary
    //
    // The `--version` filter narrows the bucket walk to a single
    // committed migration version, surfacing here once the engine
    // lands.
    if let Some(v) = version
        && !v.is_empty()
    {
        return Err(refuse_offline_only(format!(
            "live plan: explicit version filter `{v}` requires the live-plan compose engine; \
             this CLI build ships the dispatch + parsing surface only"
        )));
    }
    Err(LiveCmdError::Runtime(
        "live plan: descriptor → snapshot → classify → dispatch pipeline lands in a follow-up task; \
         this CLI build shipped the dispatch + parsing surface only. Use `djogi migrations compose` \
         today; the live-plan emitter wraps that in a forthcoming task"
            .to_string(),
    ))
}

/// Refuse the live-plan compose path when the delta carries an
/// `OfflineOnly` operation. Hoisted into a free function so the future
/// `live plan` engine wires it in by importing this helper rather than
/// re-deriving the message shape; the wider call surface here surfaces
/// the variant in the public API for the `LiveCmdError::exit_code()`
/// contract (exit 2).
///
/// Returns [`LiveCmdError::ClassificationRefused`] which maps to exit
/// code 2 per v3 §3 of the Phase 7.5 plan.
pub fn refuse_offline_only(reason: impl Into<String>) -> LiveCmdError {
    LiveCmdError::ClassificationRefused(reason.into())
}

/// Surface a validation gate failure (exit code 3). Hoisted into a
/// free function for the same reason as [`refuse_offline_only`] —
/// the gate-query loop in T11+ uses this helper directly so the exit-
/// code mapping never drifts from the v3 §3 table.
pub fn validation_failed(reason: impl Into<String>) -> LiveCmdError {
    LiveCmdError::ValidationFailed(reason.into())
}

// ── live show ─────────────────────────────────────────────────────────

/// `djogi live show` body. Resolves the plan row, reads + checksum-
/// verifies the on-disk plan file, and prints metadata + active hooks.
async fn show_cmd(plan_id_raw: &str, workspace: Option<PathBuf>) -> Result<i32, LiveCmdError> {
    let plan_id = parse_plan_id(plan_id_raw)?;
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    let mut ctx = connect(&config.database.url).await?;
    let row = fetch_row(&mut ctx, plan_id).await?;
    let path = resolve_plan_file_path(&workspace, &row);
    verify_checksum(&path, &row.plan_file_checksum)?;
    let plan = read_plan(&path)?;

    let current_index = u32::try_from(row.current_step_index).unwrap_or(0);
    let hooks = active_hooks_at_step(&plan, current_index)
        .map_err(|e| LiveCmdError::Runtime(format!("hook walker: {e}")))?;

    println!("plan_id        : {}", row.plan_id);
    println!("slug           : {}", row.slug);
    println!("classification : {}", row.classification.as_db_str());
    println!("status         : {}", row.status.as_db_str());
    println!(
        "current_step   : {} (index {})",
        row.current_step.as_deref().unwrap_or("<none>"),
        row.current_step_index,
    );
    let total = row
        .backfill_rows_total
        .map(|n| n.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    println!(
        "backfill_rows  : {} done / {} total",
        row.backfill_rows_done, total,
    );
    println!("originating    : {}", row.originating_migration.as_str(),);
    if let Some(progress) = row.last_progress_at.as_ref() {
        println!("last_progress  : {progress}");
    }
    if let Some(err) = row.last_error.as_deref() {
        println!("last_error     : {err}");
    }
    println!("plan_file      : {}", path.display());
    println!();
    println!("steps ({} total):", plan.steps.len(),);
    for step in &plan.steps {
        let marker = if (step.ordinal as i32) < row.current_step_index {
            "[done]"
        } else if (step.ordinal as i32) == row.current_step_index {
            "[curr]"
        } else {
            "[ todo]"
        };
        println!(
            "  {marker} {ordinal:>3}: {kind:?}",
            ordinal = step.ordinal,
            kind = step.kind,
        );
    }
    println!();
    println!(
        "active hooks   : dual_read={}, dual_write={}, suppress_events={}",
        hooks.dual_read.len(),
        hooks.dual_write.len(),
        hooks.side_effects_suppressed,
    );
    Ok(0)
}

// ── live run / resume ─────────────────────────────────────────────────

/// `djogi live run` body. Walks the step graph from
/// `current_step_index` forward, executing non-gate steps and pausing
/// at the first operator gate. Refuses destructive steps without
/// `--allow-destructive --justify "<reason>"`.
async fn run_cmd(
    plan_id_raw: &str,
    allow_destructive: bool,
    justify: Option<&str>,
    allow_raw_dangerous: bool,
    workspace: Option<PathBuf>,
) -> Result<i32, LiveCmdError> {
    require_justify_for_destructive(allow_destructive, justify)?;
    require_justify_for_dangerous(allow_raw_dangerous, justify)?;
    let plan_id = parse_plan_id(plan_id_raw)?;
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    let mut ctx = connect(&config.database.url).await?;
    let row = fetch_row(&mut ctx, plan_id).await?;
    assert_run_status_allows_progress(row.status)?;
    let path = resolve_plan_file_path(&workspace, &row);
    verify_checksum(&path, &row.plan_file_checksum)?;
    let _plan = read_plan(&path)?;

    // The actual step-walking executor lives behind the live-plan
    // engine entry point that ships alongside the `live plan` compose
    // body. This dispatch wires the CLI surface, checksum verify, and
    // state-conflict gates; the per-step execution loop (DDL emission,
    // gate pause / promote, transactional progress writes) is the
    // engine's job and lands in the same follow-up.
    //
    // Future gate failure path: the engine surfaces a failed gate via
    // [`validation_failed`] (exit 3). The mapping is exercised by the
    // exit-code unit tests; the production wiring lands with the
    // executor.
    let _ = allow_destructive; // silence unused-flag warning until engine lands
    let _ = allow_raw_dangerous;
    if row.last_error.is_some() {
        return Err(validation_failed(format!(
            "plan {plan_id} carries a stale `last_error`; clear it via `djogi live abandon` \
             (then re-plan) or fix the underlying cause and resume",
        )));
    }
    Err(LiveCmdError::Runtime(
        "live run: step-graph executor lands alongside the `live plan` compose entry point; \
         this CLI build shipped the dispatch + checksum verify + state-conflict gates"
            .to_string(),
    ))
}

/// `djogi live resume` body. Same shape as `run`, but additionally
/// refuses (exit 5) when the plan is at a validation / cutover /
/// finalize gate — those need `live run` (or `live finalize`).
async fn resume_cmd(plan_id_raw: &str, workspace: Option<PathBuf>) -> Result<i32, LiveCmdError> {
    let plan_id = parse_plan_id(plan_id_raw)?;
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    let mut ctx = connect(&config.database.url).await?;
    let row = fetch_row(&mut ctx, plan_id).await?;
    assert_resume_status_allows_progress(row.status)?;
    let path = resolve_plan_file_path(&workspace, &row);
    verify_checksum(&path, &row.plan_file_checksum)?;
    let _plan = read_plan(&path)?;
    Err(LiveCmdError::Runtime(
        "live resume: chunk-loop executor lands alongside the `live plan` compose entry point; \
         this CLI build shipped the dispatch + checksum verify + state-conflict gates"
            .to_string(),
    ))
}

/// Reject statuses where `live run` would be a state-machine error.
/// `Pending` / `Running` / `Paused` advance; everything else is a
/// state conflict (exit 5).
///
/// `PlanStatus` is `#[non_exhaustive]`; the trailing wildcard arm
/// classifies any future-added variant as a state conflict by default.
/// That is the conservative choice — a new variant the CLI does not
/// yet understand should refuse rather than silently advance.
fn assert_run_status_allows_progress(status: PlanStatus) -> Result<(), LiveCmdError> {
    match status {
        PlanStatus::Pending | PlanStatus::Running | PlanStatus::Paused => Ok(()),
        PlanStatus::Validating
        | PlanStatus::Cutover
        | PlanStatus::Finalizing
        | PlanStatus::Complete
        | PlanStatus::Abandoned
        | PlanStatus::Failed => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; `live run` advances only Pending / Running / Paused plans",
            status.as_db_str()
        ))),
        _ => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; this CLI build does not recognise the status",
            status.as_db_str()
        ))),
    }
}

/// Reject statuses where `live resume` would be a state-machine error.
/// Resume is more restrictive than run — it only accepts `Running` or
/// `Paused`. Operator gates (Validating / Cutover / Finalizing) need
/// `live run` to push them past the gate; terminal states refuse.
///
/// `PlanStatus` is `#[non_exhaustive]`; the trailing wildcard arm
/// classifies any future-added variant as a state conflict by default.
fn assert_resume_status_allows_progress(status: PlanStatus) -> Result<(), LiveCmdError> {
    match status {
        PlanStatus::Running | PlanStatus::Paused => Ok(()),
        PlanStatus::Pending => Err(LiveCmdError::StateConflict(
            "plan is in `pending`; use `live run` to start it (resume is for an interrupted run)"
                .to_string(),
        )),
        PlanStatus::Validating
        | PlanStatus::Cutover
        | PlanStatus::Finalizing
        | PlanStatus::Complete
        | PlanStatus::Abandoned
        | PlanStatus::Failed => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; resume is for interrupted Running / Paused plans \
             (use `live run` past gates, `live finalize` to complete, or `live abandon` to walk away)",
            status.as_db_str()
        ))),
        _ => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; this CLI build does not recognise the status",
            status.as_db_str()
        ))),
    }
}

// ── live finalize ─────────────────────────────────────────────────────

/// `djogi live finalize` body. Gated on the row being in
/// [`PlanStatus::Finalizing`]; advances every remaining
/// [`StepKind::FinalizeConstraints`](djogi::live_migrate::StepKind::FinalizeConstraints)
/// /
/// [`StepKind::CleanupLegacyState`](djogi::live_migrate::StepKind::CleanupLegacyState)
/// step, drops compatibility hooks, and promotes the row to
/// [`PlanStatus::Complete`].
async fn finalize_cmd(
    plan_id_raw: &str,
    justify: Option<&str>,
    workspace: Option<PathBuf>,
) -> Result<i32, LiveCmdError> {
    let plan_id = parse_plan_id(plan_id_raw)?;
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    let mut ctx = connect(&config.database.url).await?;
    let row = fetch_row(&mut ctx, plan_id).await?;
    assert_finalize_status(row.status)?;
    let path = resolve_plan_file_path(&workspace, &row);
    verify_checksum(&path, &row.plan_file_checksum)?;
    let _plan = read_plan(&path)?;

    // Cleanup steps drop legacy state; if any remaining cleanup step
    // is destructive, refuse without a `--justify`. The plan's step
    // graph is the source of truth; this dispatch surfaces the gate,
    // and the engine's finalize loop walks the graph.
    let _ = justify;
    Err(LiveCmdError::Runtime(
        "live finalize: cleanup-step executor lands alongside the `live plan` compose entry \
         point; this CLI build shipped the dispatch + checksum + state gate"
            .to_string(),
    ))
}

/// Reject statuses where `live finalize` would be a state-machine
/// error. Only `Finalizing` advances.
fn assert_finalize_status(status: PlanStatus) -> Result<(), LiveCmdError> {
    match status {
        PlanStatus::Finalizing => Ok(()),
        other => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; `live finalize` runs only against the `finalizing` state",
            other.as_db_str()
        ))),
    }
}

// ── live abandon ──────────────────────────────────────────────────────

/// `djogi live abandon` body. Confirmation prompt unless `--force`;
/// `--force` requires `DJOGI_ENV != production`.
async fn abandon_cmd(
    plan_id_raw: &str,
    force: bool,
    workspace: Option<PathBuf>,
) -> Result<i32, LiveCmdError> {
    let plan_id = parse_plan_id(plan_id_raw)?;
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    if force && !force_allowed_in_env() {
        return Err(LiveCmdError::ArgRefused(
            "--force refused under DJOGI_ENV=production".to_string(),
        ));
    }
    let confirmed = if force {
        true
    } else {
        match interactive_confirm_abandon(plan_id) {
            Ok(c) => c,
            Err(_) => {
                return Err(LiveCmdError::ArgRefused(
                    "failed to read confirmation; refusing without an explicit `--force`"
                        .to_string(),
                ));
            }
        }
    };
    if !confirmed {
        eprintln!("djogi live abandon: aborted; plan {plan_id} unchanged");
        return Ok(0);
    }

    let mut ctx = connect(&config.database.url).await?;
    let row = fetch_row(&mut ctx, plan_id).await?;
    assert_abandon_status(row.status)?;
    djogi::live_migrate::state::update_status(
        &mut ctx,
        plan_id,
        &row.target_database,
        &row.app_label,
        PlanStatus::Abandoned,
    )
    .await
    .map_err(|e| LiveCmdError::Runtime(format!("abandon update_status: {e}")))?;

    println!(
        "live abandon: plan {plan_id} marked abandoned (was `{}`); plan file \
         preserved on disk for audit",
        row.status.as_db_str(),
    );
    Ok(0)
}

/// Reject statuses where `live abandon` would be a no-op or rewrite
/// terminal state. `Complete` and `Abandoned` are terminal; the rest
/// can be abandoned.
fn assert_abandon_status(status: PlanStatus) -> Result<(), LiveCmdError> {
    match status {
        PlanStatus::Complete => Err(LiveCmdError::StateConflict(
            "plan is `complete`; nothing to abandon".to_string(),
        )),
        PlanStatus::Abandoned => Err(LiveCmdError::StateConflict(
            "plan is already `abandoned`".to_string(),
        )),
        _ => Ok(()),
    }
}

/// Interactive confirmation prompt for `live abandon`. Reads stdin;
/// only `y` / `yes` (case-insensitive ASCII) returns `Ok(true)`.
fn interactive_confirm_abandon(plan_id: HeerId) -> std::io::Result<bool> {
    use std::io::{BufRead, Write};
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    writeln!(
        handle,
        "WARNING: live abandon will mark plan {plan_id} as `abandoned`. Schema state \
         remains at the last completed step; the plan file stays on disk. Resume is \
         refused after abandonment — generate a fresh plan instead."
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Test driver — clap derive macros emit a parser per top-level
    /// command; the live subcommand sits under a fixture with one
    /// nested variant so the unit tests can exercise the arg parsing
    /// without depending on the full top-level CLI shape.
    #[derive(Parser, Debug)]
    struct LiveCli {
        #[command(subcommand)]
        cmd: LiveCmd,
    }

    fn parse(argv: &[&str]) -> Result<LiveCli, clap::Error> {
        let mut full = vec!["live"];
        full.extend_from_slice(argv);
        LiveCli::try_parse_from(full)
    }

    // ── parsing ──────────────────────────────────────────────────────

    #[test]
    fn live_plan_parses_without_args() {
        let parsed = parse(&["plan"]).expect("plan parses");
        match parsed.cmd {
            LiveCmd::Plan { version, .. } => assert!(version.is_none()),
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn live_plan_accepts_optional_version() {
        let parsed = parse(&["plan", "V20260428000000__demo"]).expect("plan with version parses");
        match parsed.cmd {
            LiveCmd::Plan { version, .. } => {
                assert_eq!(version.as_deref(), Some("V20260428000000__demo"));
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn live_show_requires_plan_id() {
        let err = parse(&["show"]).expect_err("show without plan_id must fail");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("plan_id") || msg.to_lowercase().contains("required"),
            "expected plan_id requirement in clap message: {msg}",
        );
    }

    #[test]
    fn live_show_parses_plan_id() {
        let parsed = parse(&["show", "12345"]).expect("show with plan_id parses");
        match parsed.cmd {
            LiveCmd::Show { plan_id, .. } => assert_eq!(plan_id, "12345"),
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn live_run_accepts_allow_destructive_with_justify() {
        let parsed = parse(&[
            "run",
            "12345",
            "--allow-destructive",
            "--justify",
            "rotate keys for incident IR-7",
        ])
        .expect("run with destructive + justify parses");
        match parsed.cmd {
            LiveCmd::Run {
                plan_id,
                allow_destructive,
                justify,
                allow_raw_dangerous,
                ..
            } => {
                assert_eq!(plan_id, "12345");
                assert!(allow_destructive);
                assert_eq!(justify.as_deref(), Some("rotate keys for incident IR-7"));
                assert!(!allow_raw_dangerous);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn live_run_accepts_allow_raw_dangerous_with_justify() {
        let parsed = parse(&[
            "run",
            "67890",
            "--allow-raw-dangerous",
            "--justify",
            "operator runbook RB-12",
        ])
        .expect("run with allow-raw-dangerous parses");
        match parsed.cmd {
            LiveCmd::Run {
                allow_raw_dangerous,
                justify,
                ..
            } => {
                assert!(allow_raw_dangerous);
                assert_eq!(justify.as_deref(), Some("operator runbook RB-12"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn live_resume_parses() {
        let parsed = parse(&["resume", "55"]).expect("resume parses");
        assert!(matches!(parsed.cmd, LiveCmd::Resume { .. }));
    }

    #[test]
    fn live_finalize_accepts_justify() {
        let parsed = parse(&["finalize", "55", "--justify", "drop legacy"])
            .expect("finalize with justify parses");
        match parsed.cmd {
            LiveCmd::Finalize {
                justify, plan_id, ..
            } => {
                assert_eq!(plan_id, "55");
                assert_eq!(justify.as_deref(), Some("drop legacy"));
            }
            other => panic!("expected Finalize, got {other:?}"),
        }
    }

    #[test]
    fn live_abandon_accepts_force() {
        let parsed = parse(&["abandon", "12345", "--force"]).expect("abandon with force parses");
        match parsed.cmd {
            LiveCmd::Abandon { force, plan_id, .. } => {
                assert!(force);
                assert_eq!(plan_id, "12345");
            }
            other => panic!("expected Abandon, got {other:?}"),
        }
    }

    // ── helpers ──────────────────────────────────────────────────────

    #[test]
    fn justify_is_empty_handles_none_and_blank() {
        assert!(justify_is_empty(None));
        assert!(justify_is_empty(Some("")));
        assert!(justify_is_empty(Some("   ")));
        assert!(!justify_is_empty(Some("real reason")));
    }

    #[test]
    fn require_justify_for_destructive_refuses_without_reason() {
        let err = require_justify_for_destructive(true, None).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        let err = require_justify_for_destructive(true, Some("   ")).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        // Without `--allow-destructive`, missing `--justify` is fine.
        require_justify_for_destructive(false, None).unwrap();
        // With both set, accepts.
        require_justify_for_destructive(true, Some("rotate keys")).unwrap();
    }

    #[test]
    fn require_justify_for_dangerous_refuses_without_reason() {
        let err = require_justify_for_dangerous(true, None).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        require_justify_for_dangerous(false, None).unwrap();
        require_justify_for_dangerous(true, Some("runbook")).unwrap();
    }

    #[test]
    fn parse_plan_id_accepts_decimal() {
        let id = parse_plan_id("12345").unwrap();
        assert_eq!(id.as_i64(), 12345);
    }

    #[test]
    fn parse_plan_id_rejects_garbage() {
        let err = parse_plan_id("not-a-number").unwrap_err();
        assert!(matches!(err, LiveCmdError::MalformedPlanId(_)));
    }

    // ── exit-code mapping ─────────────────────────────────────────────

    #[test]
    fn exit_code_runtime_maps_to_one() {
        assert_eq!(LiveCmdError::Runtime("x".to_string()).exit_code(), 1);
        assert_eq!(LiveCmdError::ArgRefused("x".to_string()).exit_code(), 1);
        assert_eq!(
            LiveCmdError::MalformedPlanId("x".to_string()).exit_code(),
            1
        );
    }

    #[test]
    fn exit_code_classification_refused_maps_to_two() {
        assert_eq!(
            LiveCmdError::ClassificationRefused("offline only".to_string()).exit_code(),
            2,
        );
    }

    #[test]
    fn exit_code_validation_failed_maps_to_three() {
        assert_eq!(
            LiveCmdError::ValidationFailed("count > 0".to_string()).exit_code(),
            3,
        );
    }

    #[test]
    fn exit_code_checksum_drift_maps_to_four() {
        assert_eq!(
            LiveCmdError::ChecksumDrift("mismatch".to_string()).exit_code(),
            4,
        );
    }

    #[test]
    fn exit_code_state_conflict_maps_to_five() {
        assert_eq!(
            LiveCmdError::StateConflict("complete".to_string()).exit_code(),
            5,
        );
    }

    // ── status-gate helpers ──────────────────────────────────────────

    #[test]
    fn assert_run_status_accepts_pending_running_paused() {
        assert!(assert_run_status_allows_progress(PlanStatus::Pending).is_ok());
        assert!(assert_run_status_allows_progress(PlanStatus::Running).is_ok());
        assert!(assert_run_status_allows_progress(PlanStatus::Paused).is_ok());
    }

    #[test]
    fn assert_run_status_refuses_terminal_and_gates() {
        for status in [
            PlanStatus::Validating,
            PlanStatus::Cutover,
            PlanStatus::Finalizing,
            PlanStatus::Complete,
            PlanStatus::Abandoned,
            PlanStatus::Failed,
        ] {
            let err = assert_run_status_allows_progress(status)
                .expect_err("non-progressable status must refuse");
            assert!(matches!(err, LiveCmdError::StateConflict(_)));
        }
    }

    #[test]
    fn assert_resume_status_distinguishes_pending_from_terminal() {
        // Pending is a state conflict for `resume` (you'd use `run`).
        let err = assert_resume_status_allows_progress(PlanStatus::Pending)
            .expect_err("pending must refuse");
        match err {
            LiveCmdError::StateConflict(msg) => assert!(msg.contains("pending")),
            other => panic!("expected StateConflict, got {other:?}"),
        }
        // Running and Paused accept.
        assert!(assert_resume_status_allows_progress(PlanStatus::Running).is_ok());
        assert!(assert_resume_status_allows_progress(PlanStatus::Paused).is_ok());
        // Everything else refuses.
        for status in [
            PlanStatus::Validating,
            PlanStatus::Cutover,
            PlanStatus::Finalizing,
            PlanStatus::Complete,
            PlanStatus::Abandoned,
            PlanStatus::Failed,
        ] {
            let err = assert_resume_status_allows_progress(status)
                .expect_err("non-resumable status must refuse");
            assert!(matches!(err, LiveCmdError::StateConflict(_)));
        }
    }

    #[test]
    fn assert_finalize_status_accepts_only_finalizing() {
        assert!(assert_finalize_status(PlanStatus::Finalizing).is_ok());
        for status in [
            PlanStatus::Pending,
            PlanStatus::Running,
            PlanStatus::Paused,
            PlanStatus::Validating,
            PlanStatus::Cutover,
            PlanStatus::Complete,
            PlanStatus::Abandoned,
            PlanStatus::Failed,
        ] {
            let err = assert_finalize_status(status).expect_err("non-finalizing must refuse");
            assert!(matches!(err, LiveCmdError::StateConflict(_)));
        }
    }

    #[test]
    fn assert_abandon_status_refuses_terminal_states() {
        // Complete and Abandoned are the only state-conflict cases.
        assert!(matches!(
            assert_abandon_status(PlanStatus::Complete).unwrap_err(),
            LiveCmdError::StateConflict(_)
        ));
        assert!(matches!(
            assert_abandon_status(PlanStatus::Abandoned).unwrap_err(),
            LiveCmdError::StateConflict(_)
        ));
        // Every other status is a valid abandonment target.
        for status in [
            PlanStatus::Pending,
            PlanStatus::Running,
            PlanStatus::Paused,
            PlanStatus::Validating,
            PlanStatus::Cutover,
            PlanStatus::Finalizing,
            PlanStatus::Failed,
        ] {
            assert!(assert_abandon_status(status).is_ok(), "{status:?} accepts");
        }
    }

    // ── env gate ─────────────────────────────────────────────────────

    #[test]
    fn force_allowed_when_djogi_env_unset() {
        // SAFETY: tests run with --test-threads=1.
        let prior = std::env::var("DJOGI_ENV").ok();
        unsafe { std::env::remove_var("DJOGI_ENV") };
        assert!(force_allowed_in_env());
        unsafe { std::env::set_var("DJOGI_ENV", "development") };
        assert!(force_allowed_in_env());
        unsafe { std::env::set_var("DJOGI_ENV", "PRODUCTION") };
        assert!(
            !force_allowed_in_env(),
            "case-insensitive production must refuse"
        );
        unsafe { std::env::set_var("DJOGI_ENV", "production") };
        assert!(!force_allowed_in_env());
        // Restore.
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    // ── PlanFileError → LiveCmdError mapping ─────────────────────────

    #[test]
    fn plan_file_checksum_mismatch_maps_to_drift() {
        let pfe = PlanFileError::ChecksumMismatch {
            path: PathBuf::from("/tmp/x.json"),
            expected: "V1:0".to_string(),
            actual: "V1:1".to_string(),
        };
        let err: LiveCmdError = pfe.into();
        assert_eq!(err.exit_code(), 4, "checksum mismatch must exit 4");
    }

    #[test]
    fn plan_file_io_maps_to_runtime() {
        let pfe = PlanFileError::NotFound(PathBuf::from("/missing"));
        let err: LiveCmdError = pfe.into();
        assert_eq!(err.exit_code(), 1);
    }
}
