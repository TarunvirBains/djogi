//! `djogi live` — operator surface for live migrations.
//! Six subcommands materialise the operator-facing contract for the
//! expand → backfill → flip → contract sequence the live-plan layer
//! drives:
//! - `live plan` — generate plan files for pending schema deltas
//! classified [`OnlineSafetyClassification::ExpandContract`](djogi::live_migrate::OnlineSafetyClassification::ExpandContract).
//! - `live show` — render plan-file metadata + persisted runtime state
//! side by side, plus the active hook snapshot at the current step.
//! - `live run` — drive the plan forward until the next operator gate,
//! refusing destructive work without `--allow-destructive`
//! `--justify "<reason>"`.
//! - `live resume` — pick up after an interruption; reads
//! `backfill_rows_done` from the row and continues at the same step.
//! - `live finalize` — execute remaining
//! [`StepKind::FinalizeConstraints`](djogi::live_migrate::StepKind::FinalizeConstraints)
//! /
//! [`StepKind::CleanupLegacyState`](djogi::live_migrate::StepKind::CleanupLegacyState)
//! steps, drop compatibility hooks, and promote the row to
//! [`PlanStatus::Complete`](djogi::live_migrate::PlanStatus::Complete).
//! - `live abandon` — terminal opt-out; gated on confirmation OR
//! `--force` plus a non-production `DJOGI_ENV`.
//! # Exit codes
//! Per of the plan:
//! | Code | Meaning |
//! |------|---------|
//! | 0 | Success — operator may invoke the next subcommand. |
//! | 1 | Step failed — generic runtime error (config / network / SQL / I/O). |
//! | 2 | Classification refused to plan — the delta is `OfflineOnly`. |
//! | 3 | Validation checkpoint failed — gate query disagreed. |
//! | 4 | Plan-file checksum drift — the file was edited after start. |
//! | 5 | Plan state conflicts with request (e.g. `run` on `complete`). |
//! # Out of scope
//! - **Live-DB integration tests.** This module ships clap parsing,
//! helpers, and exit-code mapping; end-to-end coverage lives in the
//! integration suite.
//! - **`djogi_codec_recode(...)`** — still a placeholder.
//! - **`djogi_schema_migrations.justification` persistence.** Adding
//! the column requires a separate ALTER TABLE migration; this module
//! accepts `--justify` and routes the value through to the runner,
//! but the actual column write lands in a follow-up phase.

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use clap::Subcommand;
use djogi::__bypass::RawAccessExt as _;
use djogi::config::DjogiConfig;
use djogi::context::DjogiContext;
use djogi::live_migrate::compose::StepResult;
use djogi::live_migrate::{
    DaemonConfig, DaemonError, LivePlanRow, PlanFileError, PlanStatus, active_hooks_at_step,
    plan_path, read_plan, run_daemon, verify_checksum,
};
use djogi::pg::pool::DjogiPool;
use djogi::types::HeerId;

// ── Subcommand surface ────────────────────────────────────────────────

/// Operator surface for the live-migration runner. Every subcommand
/// resolves to an [`ExitCode`] via [`dispatch`].
#[derive(Debug, Clone, Subcommand)]
pub enum LiveCmd {
    /// Generate plan file(s) for pending schema deltas classified
    /// `ExpandContract`. Refuses (exit 2) when the delta is
    /// `OfflineOnly`; reports "no live plan needed" (exit 0) when the
    /// delta is fully `OnlineSafe` and the regular runner can
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
        /// hand-edited (per amendment). Always paired with
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
        /// Allow destructive steps to execute on resume. Required when
        /// the plan contains a DROP / TRUNCATE-class step that the
        /// resume would reach. Pairs with `--justify`.
        #[arg(long, default_value_t = false)]
        allow_destructive: bool,
        /// Operator justification recorded alongside any destructive
        /// step. Required when `--allow-destructive` is set.
        #[arg(long)]
        justify: Option<String>,
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
    /// Long-running daemon that resumes stale `BackfillChunked` /
    /// `ValidateBackfill` steps. Triple-gated — refuses on
    /// `DJOGI_ENV=production`, refuses on a non-localhost
    /// `DATABASE_URL` unless `--allow-non-localhost` is set, and never
    /// auto-advances past an operator gate (`CutoverReads` /
    /// `CutoverWrites` / `FinalizeConstraints` stay manual via
    /// `live run` / `live finalize`).
    /// Exits cleanly on SIGTERM / SIGINT; per-plan failures inside the
    /// loop are logged and the daemon continues.
    Daemon {
        /// Interval between candidate-row scans. Accepts humantime
        /// single-unit durations: `30s`, `5m`, `2h`, `1d`. Default 30s.
        #[arg(long, default_value = "30s", value_parser = parse_humantime_duration)]
        poll_interval: std::time::Duration,
        /// A row is treated as stale (and therefore claimable) when
        /// its `last_progress_at` is older than this duration. Accepts
        /// the same humantime forms as `--poll-interval`. Default 10m.
        #[arg(long, default_value = "10m", value_parser = parse_humantime_duration)]
        claim_stale_after: std::time::Duration,
        /// Allow the daemon to drive a non-localhost database. Mirrors
        /// the seed gate; the production-environment gate
        /// (`DJOGI_ENV=production`) and production-profile gate
        /// (`Djogi.toml::profile = "production"`) still refuse
        /// regardless.
        #[arg(long, default_value_t = false)]
        allow_non_localhost: bool,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

// ── Errors ────────────────────────────────────────────────────────────

/// Errors raised by the `live` subcommand. Each variant carries a
/// dedicated exit-code mapping; conversion lives in
/// [`LiveCmdError::exit_code`].
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
    /// Map the error to the inline-decision exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            LiveCmdError::Runtime(_)
            | LiveCmdError::ArgRefused(_)
            | LiveCmdError::MalformedPlanId(_)
            | LiveCmdError::PlanNotFound(_) => 1,
            LiveCmdError::ClassificationRefused(_) => 2,
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
        LiveCmd::Resume {
            plan_id,
            allow_destructive,
            justify,
            workspace,
        } => resume_cmd(&plan_id, allow_destructive, justify.as_deref(), workspace).await,
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
        LiveCmd::Daemon {
            poll_interval,
            claim_stale_after,
            allow_non_localhost,
            workspace,
        } => {
            daemon_cmd(
                poll_interval,
                claim_stale_after,
                allow_non_localhost,
                workspace,
            )
            .await
        }
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
/// `--justify "<reason>"` value. The pairing rule comes from
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
/// when that column lands in a follow-up migration.
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

/// Pre-flight scan over a loaded plan. Refuses with
/// [`LivePlanGate::DestructiveWithoutJustify`] when the plan contains
/// at least one step whose execution would emit destructive SQL but
/// the operator did not pass BOTH `--allow-destructive` and a
/// non-empty `--justify "<reason>"`.
/// The scan is conservative: when the plan's destructive steps all
/// pre-date the row's `current_step_index`, the gate still fires. This
/// is intentional — a re-run that resumes past a destructive step into
/// non-destructive territory still required `--allow-destructive` on
/// the original invocation, and the audit trail benefits from the gate
/// being uniformly recorded for any plan that ever contained one.
fn require_destructive_gate_for_plan(
    plan: &djogi::live_migrate::LivePlan,
    allow_destructive: bool,
    justify: Option<&str>,
) -> Result<(), LiveCmdError> {
    if !plan.has_destructive_steps() {
        return Ok(());
    }
    if !allow_destructive {
        return Err(LiveCmdError::ArgRefused(
            "plan contains a destructive step (DROP / TRUNCATE class); \
    pass `--allow-destructive --justify \"<reason>\"` to proceed"
                .to_string(),
        ));
    }
    if justify_is_empty(justify) {
        return Err(LiveCmdError::ArgRefused(
            "plan contains a destructive step; `--allow-destructive` requires \
    `--justify \"<reason>\"`"
                .to_string(),
        ));
    }
    Ok(())
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

/// Parse a humantime-style single-unit duration string into a
/// [`std::time::Duration`]. Supported units (suffix character):
/// | Suffix | Meaning | Multiplier |
/// |--------|----------|------------|
/// | `s` | seconds | 1 |
/// | `m` | minutes | 60 |
/// | `h` | hours | 3600 |
/// | `d` | days | 86_400 |
/// `min` is also accepted as an alias for `m` so operators familiar
/// with `humantime`-style strings can write `10min`. The numeric prefix
/// must be one or more ASCII digits; the unit is one of the suffixes
/// above. Trailing whitespace is rejected — the input is the entire
/// flag value.
/// Implementation note: no regex engine, no third-party humantime
/// crate. Byte-level walk per project policy.
/// Returns the input echoed back inside the error string so operators
/// see what they typed.
fn parse_humantime_duration(s: &str) -> Result<std::time::Duration, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "empty duration string `{s}`; expected e.g. `30s` / `5m` / `2h` / `1d` / `10min`"
        ));
    }
    let bytes = trimmed.as_bytes();
    // Walk leading ASCII digits.
    let mut i = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return Err(format!(
            "duration `{s}` must start with one or more ASCII digits"
        ));
    }
    let digits = &trimmed[..i];
    let unit = &trimmed[i..];
    let value: u64 = digits
        .parse()
        .map_err(|e| format!("duration `{s}`: numeric prefix `{digits}` overflows u64: {e}"))?;
    let secs: u64 = match unit {
        "s" => value,
        "m" | "min" => value
            .checked_mul(60)
            .ok_or_else(|| format!("duration `{s}` overflows u64 seconds"))?,
        "h" => value
            .checked_mul(3_600)
            .ok_or_else(|| format!("duration `{s}` overflows u64 seconds"))?,
        "d" => value
            .checked_mul(86_400)
            .ok_or_else(|| format!("duration `{s}` overflows u64 seconds"))?,
        other => {
            return Err(format!(
                "duration `{s}`: unknown unit `{other}`; expected `s` / `m` / `min` / `h` / `d`"
            ));
        }
    };
    Ok(std::time::Duration::from_secs(secs))
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
    djogi::pg::preflight::check_postgres_version(&pool)
        .await
        .map_err(|e| LiveCmdError::Runtime(format!("support boundary: {e}")))?;
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
/// Builds the descriptor projection, diffs against the persisted
/// snapshots, and routes every `ExpandContract`-classified operation
/// through [`djogi::live_migrate::dispatch_pattern`]. Emits a plan
/// file plus a `djogi_live_plans` row per bucket that needed one.
/// Refuses with exit code 2 when the delta carries any `OfflineOnly`
/// classification — the operator must hand-edit those rather than
/// route through `live`. Reports "no live plan needed" with exit code
/// 0 when every classification is `OnlineSafe` (the regular
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
    // When the engine lands, the body becomes:
    // 1. project_from_inventory + load_snapshot per bucket
    // 2. diff_bucket_maps → Vec<SchemaDelta>
    // 3. classify_delta per bucket; route OfflineOnly through
    // [`refuse_offline_only`] (exit 2)
    // 4. for each ExpandContract op: dispatch_pattern → Vec<Step>
    // 5. write_plan + insert_row per bucket
    // 6. print summary
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
/// Returns [`LiveCmdError::ClassificationRefused`] which maps to exit
/// code 2 per of the plan.
pub fn refuse_offline_only(reason: impl Into<String>) -> LiveCmdError {
    LiveCmdError::ClassificationRefused(reason.into())
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

    println!("plan_id  : {}", row.plan_id);
    println!("slug   : {}", row.slug);
    println!("classification : {}", row.classification.as_db_str());
    println!("status   : {}", row.status.as_db_str());
    println!(
        "current_step : {} (index {})",
        row.current_step.as_deref().unwrap_or("<none>"),
        row.current_step_index,
    );
    let total = row
        .backfill_rows_total
        .map(|n| n.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    println!(
        "backfill_rows : {} done / {} total",
        row.backfill_rows_done, total,
    );
    println!("originating : {}", row.originating_migration.as_str(),);
    if let Some(progress) = row.last_progress_at.as_ref() {
        println!("last_progress : {progress}");
    }
    if let Some(err) = row.last_error.as_deref() {
        println!("last_error  : {err}");
    }
    println!("plan_file  : {}", path.display());
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
            " {marker} {ordinal:>3}: {kind:?}",
            ordinal = step.ordinal,
            kind = step.kind,
        );
    }
    println!();
    println!(
        "active hooks : dual_read={}, dual_write={}, suppress_events={}",
        hooks.dual_read.len(),
        hooks.dual_write.len(),
        hooks.side_effects_suppressed,
    );
    Ok(0)
}

// ── live run / resume ─────────────────────────────────────────────────

/// `djogi live run` body. Executes all steps in the plan starting
/// from step 0. Pauses at the first OperatorGate step and records
/// progress via checkpoint(). Refuses destructive steps without
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
    let plan = read_plan(&path)?;

    // Walk the plan ahead of time and refuse any destructive step
    // (CleanupLegacyState, or RunReversibleSchemaOp whose up_sql drops
    // state) unless the operator passed BOTH `--allow-destructive` and a
    // non-empty `--justify`. Without this proactive scan, a plan whose
    // tail contains a DROP COLUMN would silently execute as soon as the
    // runner reached that step. The check fires before any side effect
    // so the operator sees the refusal and can re-invoke with the gate
    // pair.
    require_destructive_gate_for_plan(&plan, allow_destructive, justify)?;

    // The actual step-walking executor lives behind the live-plan
    // engine entry point that ships alongside the `live plan` compose
    // body. This dispatch wires the CLI surface, checksum verify, and
    // state-conflict gates; the per-step execution loop (DDL emission,
    // gate pause / promote, transactional progress writes) is the
    // engine's job and lands in the same follow-up.
    // Execute the plan via the live-plan engine. The operator-supplied
    // `allow_destructive` / `justify` flow through to the engine-side
    // gate as well as the CLI pre-flight above (belt and suspenders).
    match djogi::live_migrate::executor::run_plan(
        &mut ctx,
        path,
        0,
        false,
        allow_destructive,
        justify,
    )
    .await
    {
        Ok(result) => match result {
            StepResult::Completed => {
                println!("live run: plan {plan_id} completed successfully");
                Ok(0)
            }
            StepResult::Paused => {
                println!(
                    "live run: paused at operator gate; resume with `djogi live run {plan_id}`"
                );
                Ok(0)
            }
            StepResult::Partial {
                rows_done,
                rows_total,
            } => {
                if rows_total > 0 {
                    let pct = (rows_done as f64 / rows_total as f64) * 100.0;
                    println!(
                        "live run: backfill progress {rows_done}/{rows_total} ({pct:.1}%); resume with `djogi live resume {plan_id}`"
                    );
                } else {
                    println!(
                        "live run: backfill interrupted after {rows_done} rows; resume with `djogi live resume {plan_id}`"
                    );
                }
                Ok(0)
            }
        },
        Err(e) => Err(LiveCmdError::Runtime(format!("executor error: {e}"))),
    }
}

/// `djogi live resume` body. Same shape as `run`, but additionally
/// refuses (exit 5) when the plan is at a validation / cutover /
/// finalize gate — those need `live run` (or `live finalize`).
/// A resumed plan can reach a destructive cleanup step, so it carries
/// the same `--allow-destructive` / `--justify` gate as `live run`:
/// the CLI pre-flight pairs the two flags, and the engine enforces the
/// destructive gate before executing any DROP-class step.
async fn resume_cmd(
    plan_id_raw: &str,
    allow_destructive: bool,
    justify: Option<&str>,
    workspace: Option<PathBuf>,
) -> Result<i32, LiveCmdError> {
    require_justify_for_destructive(allow_destructive, justify)?;
    let plan_id = parse_plan_id(plan_id_raw)?;
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    let mut ctx = connect(&config.database.url).await?;
    let row = fetch_row(&mut ctx, plan_id).await?;
    assert_resume_status_allows_progress(row.status)?;
    let path = resolve_plan_file_path(&workspace, &row);
    verify_checksum(&path, &row.plan_file_checksum)?;
    let _plan = read_plan(&path)?;
    // Resume execution from current step
    let start_idx = u32::try_from(row.current_step_index).unwrap_or(0);
    match djogi::live_migrate::executor::run_plan(
        &mut ctx,
        path,
        start_idx,
        true,
        allow_destructive,
        justify,
    )
    .await
    {
        Ok(result) => match result {
            StepResult::Completed => {
                println!("live resume: plan {plan_id} completed successfully");
                Ok(0)
            }
            StepResult::Paused => {
                println!(
                    "live resume: paused at operator gate; resume with `djogi live run {plan_id}`"
                );
                Ok(0)
            }
            StepResult::Partial {
                rows_done,
                rows_total,
            } => {
                if rows_total > 0 {
                    let pct = (rows_done as f64 / rows_total as f64) * 100.0;
                    println!(
                        "live resume: backfill progress {rows_done}/{rows_total} ({pct:.1}%); resume with `djogi live resume {plan_id}`"
                    );
                } else {
                    println!(
                        "live resume: backfill interrupted after {rows_done} rows; resume with `djogi live resume {plan_id}`"
                    );
                }
                Ok(0)
            }
        },
        Err(e) => Err(LiveCmdError::Runtime(format!("executor error: {e}"))),
    }
}

/// Reject statuses where `live run` would be a state-machine error.
/// Only `Pending` and `Running` advance — `Paused` is the operator's
/// explicit checkpoint state and requires `live resume` to re-enter
/// the run loop. Every other state is a state conflict (exit 5).
/// `PlanStatus` is `#[non_exhaustive]`; the trailing wildcard arm
/// classifies any future-added variant as a state conflict by default.
/// That is the conservative choice — a new variant the CLI does not
/// yet understand should refuse rather than silently advance.
fn assert_run_status_allows_progress(status: PlanStatus) -> Result<(), LiveCmdError> {
    match status {
        PlanStatus::Pending | PlanStatus::Running => Ok(()),
        PlanStatus::Paused => Err(LiveCmdError::StateConflict(
            "plan is in `paused`; use `live resume` to re-enter the run loop \
    (paused is an explicit operator checkpoint and `live run` does \
    not auto-advance through it)"
                .to_string(),
        )),
        PlanStatus::Validating
        | PlanStatus::Cutover
        | PlanStatus::Finalizing
        | PlanStatus::Complete
        | PlanStatus::Abandoned
        | PlanStatus::Failed => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; `live run` advances only Pending / Running plans",
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
/// Requires `--justify "<reason>"`; the cleanup phase is destructive by
/// nature. The `live finalize` invocation is itself the operator's
/// destructive opt-in, so it passes `allow_destructive = true` to the
/// engine but still demands a non-empty justification.
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
    // `live finalize` runs the contract (cleanup) phase, which is
    // destructive by nature. Require a justification just as `live run
    // --allow-destructive` does; treat the finalize invocation itself as
    // the operator's destructive opt-in.
    let justify_present = justify.map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !justify_present {
        return Err(LiveCmdError::ArgRefused(
            "live finalize runs destructive cleanup steps; pass \
    --justify \"<reason>\""
                .to_string(),
        ));
    }
    let path = resolve_plan_file_path(&workspace, &row);
    verify_checksum(&path, &row.plan_file_checksum)?;
    let _plan = read_plan(&path)?;
    // Resume execution from current step. `live finalize` implies the
    // destructive opt-in (5th arg `true`); the engine still requires the
    // non-empty `justify` checked above.
    let start_idx = u32::try_from(row.current_step_index).unwrap_or(0);
    match djogi::live_migrate::executor::run_plan(&mut ctx, path, start_idx, true, true, justify)
        .await
    {
        Ok(result) => match result {
            StepResult::Completed => {
                println!("live finalize: plan {plan_id} completed successfully");
                Ok(0)
            }
            StepResult::Paused => {
                println!(
                    "live finalize: paused at operator gate; resume with `djogi live run {plan_id}`"
                );
                Ok(0)
            }
            StepResult::Partial {
                rows_done,
                rows_total,
            } => {
                if rows_total > 0 {
                    let pct = (rows_done as f64 / rows_total as f64) * 100.0;
                    println!(
                        "live finalize: backfill progress {rows_done}/{rows_total} ({pct:.1}%); resume with `djogi live finalize {plan_id}`"
                    );
                } else {
                    println!(
                        "live finalize: backfill interrupted after {rows_done} rows; resume with `djogi live finalize {plan_id}`"
                    );
                }
                Ok(0)
            }
        },
        Err(e) => Err(LiveCmdError::Runtime(format!("executor error: {e}"))),
    }
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
/// terminal state. `Complete`, `Abandoned`, and `Failed` are all
/// terminal per the v3 plan §3 state machine; the rest (Pending,
/// Running, Paused, Validating, Cutover, Finalizing) can be abandoned.
/// `Failed` is terminal: a failed plan documents the failure for the
/// audit trail and should not be silently overwritten with `abandoned`
/// by the CLI. Operators recovering from a failed plan generate a
/// fresh plan after addressing the underlying cause.
fn assert_abandon_status(status: PlanStatus) -> Result<(), LiveCmdError> {
    match status {
        PlanStatus::Complete => Err(LiveCmdError::StateConflict(
            "plan is `complete`; nothing to abandon".to_string(),
        )),
        PlanStatus::Abandoned => Err(LiveCmdError::StateConflict(
            "plan is already `abandoned`".to_string(),
        )),
        PlanStatus::Failed => Err(LiveCmdError::StateConflict(
            "plan is `failed`; the failure is recorded for audit and the \
    plan is terminal — generate a fresh plan after addressing the \
    underlying cause"
                .to_string(),
        )),
        PlanStatus::Pending
        | PlanStatus::Running
        | PlanStatus::Paused
        | PlanStatus::Validating
        | PlanStatus::Cutover
        | PlanStatus::Finalizing => Ok(()),
        _ => Err(LiveCmdError::StateConflict(format!(
            "plan is in `{}`; this CLI build does not recognise the status",
            status.as_db_str()
        ))),
    }
}

// ── live daemon ───────────────────────────────────────────────────────

/// `djogi live daemon` body. Builds the [`DaemonConfig`] from operator
/// flags and routes through [`run_daemon`]. The daemon's triple-gate
/// (production env + localhost) fires inside `run_daemon`; this body
/// only translates the result back to a CLI exit code.
/// Returns `0` on a clean SIGTERM / SIGINT shutdown ([`DaemonError::Shutdown`])
/// the daemon's only successful exit path. Production / localhost
/// gate refusals map to [`LiveCmdError::ArgRefused`] (exit 1 — the
/// gate fired before any side effect, but the operator-visible
/// difference between "gate refused" and "ran but failed" is captured
/// in the error message rather than the exit code).
async fn daemon_cmd(
    poll_interval: std::time::Duration,
    claim_stale_after: std::time::Duration,
    allow_non_localhost: bool,
    workspace: Option<PathBuf>,
) -> Result<i32, LiveCmdError> {
    let workspace = resolve_workspace(workspace);
    let config = load_config(&workspace)?;
    let cfg = DaemonConfig {
        poll_interval,
        claim_stale_after,
        allow_non_localhost,
        database_url: config.database.url.clone(),
        host: hostname_for_claim(),
        pid: i64::from(std::process::id()),
        profile: config.profile.clone(),
        workspace_root: workspace.to_path_buf(),
    };
    let mut ctx = connect(&config.database.url).await?;
    match run_daemon(&mut ctx, cfg).await {
  Ok(()) => Ok(0),
  Err(DaemonError::Shutdown) => Ok(0),
  Err(DaemonError::NotLocalhost) => Err(LiveCmdError::ArgRefused(
   "live daemon refused: not running on localhost (pass --allow-non-localhost to override)"
   .to_string(),
  )),
  Err(DaemonError::Production) => Err(LiveCmdError::ArgRefused(
   "live daemon refused: DJOGI_ENV=production".to_string(),
  )),
  Err(DaemonError::Backfill(e)) => {
   Err(LiveCmdError::Runtime(format!("daemon backfill: {e}")))
  }
  Err(DaemonError::Database(e)) => Err(LiveCmdError::Runtime(format!("daemon db: {e}"))),
  Err(other) => Err(LiveCmdError::Runtime(format!("daemon: {other}"))),
 }
}

/// Read the running host's name for the daemon's claim columns. Falls
/// back to `"unknown"` so a missing `HOSTNAME` env var produces a
/// recognisable diagnostic in `live show` rather than an empty string.
fn hostname_for_claim() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
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

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior_hostname: Option<String>,
        prior_djogi_env: Option<String>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                _lock: crate::test_env_lock(),
                prior_hostname: std::env::var("HOSTNAME").ok(),
                prior_djogi_env: std::env::var("DJOGI_ENV").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior_hostname {
                Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
                None => unsafe { std::env::remove_var("HOSTNAME") },
            }
            match &self.prior_djogi_env {
                Some(value) => unsafe { std::env::set_var("DJOGI_ENV", value) },
                None => unsafe { std::env::remove_var("DJOGI_ENV") },
            }
        }
    }
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

    #[test]
    fn live_daemon_parses_with_default_intervals() {
        let parsed = parse(&["daemon"]).expect("daemon parses with no args");
        match parsed.cmd {
            LiveCmd::Daemon {
                poll_interval,
                claim_stale_after,
                allow_non_localhost,
                ..
            } => {
                assert_eq!(
                    poll_interval,
                    std::time::Duration::from_secs(30),
                    "default poll interval is 30s",
                );
                assert_eq!(
                    claim_stale_after,
                    std::time::Duration::from_secs(600),
                    "default stale threshold is 10 minutes",
                );
                assert!(
                    !allow_non_localhost,
                    "default refuses non-localhost connections",
                );
            }
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    #[test]
    fn live_daemon_accepts_custom_intervals() {
        let parsed = parse(&[
            "daemon",
            "--poll-interval",
            "5s",
            "--claim-stale-after",
            "1m",
            "--allow-non-localhost",
        ])
        .expect("daemon with overrides parses");
        match parsed.cmd {
            LiveCmd::Daemon {
                poll_interval,
                claim_stale_after,
                allow_non_localhost,
                ..
            } => {
                assert_eq!(poll_interval, std::time::Duration::from_secs(5));
                assert_eq!(claim_stale_after, std::time::Duration::from_secs(60));
                assert!(allow_non_localhost);
            }
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    #[test]
    fn live_daemon_accepts_humantime_minutes_and_hours() {
        // The humantime parser accepts `m` / `min` / `h` / `d`. Pin the
        // common operator-runbook forms.
        let parsed = parse(&[
            "daemon",
            "--poll-interval",
            "10min",
            "--claim-stale-after",
            "2h",
        ])
        .expect("daemon with humantime durations parses");
        match parsed.cmd {
            LiveCmd::Daemon {
                poll_interval,
                claim_stale_after,
                ..
            } => {
                assert_eq!(poll_interval, std::time::Duration::from_secs(600));
                assert_eq!(claim_stale_after, std::time::Duration::from_secs(7200));
            }
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    #[test]
    fn live_daemon_accepts_workspace_override() {
        let parsed = parse(&["daemon", "--workspace", "/tmp/example"])
            .expect("daemon with --workspace parses");
        match parsed.cmd {
            LiveCmd::Daemon { workspace, .. } => {
                assert_eq!(
                    workspace.as_deref(),
                    Some(std::path::Path::new("/tmp/example")),
                );
            }
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    // ── parse_humantime_duration (Finding 7) ──────────────────────────

    #[test]
    fn parse_humantime_duration_accepts_seconds() {
        assert_eq!(
            parse_humantime_duration("30s").unwrap(),
            std::time::Duration::from_secs(30),
        );
        assert_eq!(
            parse_humantime_duration("0s").unwrap(),
            std::time::Duration::from_secs(0),
        );
    }

    #[test]
    fn parse_humantime_duration_accepts_minutes_and_hours_and_days() {
        assert_eq!(
            parse_humantime_duration("5m").unwrap(),
            std::time::Duration::from_secs(300),
        );
        assert_eq!(
            parse_humantime_duration("10min").unwrap(),
            std::time::Duration::from_secs(600),
        );
        assert_eq!(
            parse_humantime_duration("2h").unwrap(),
            std::time::Duration::from_secs(7_200),
        );
        assert_eq!(
            parse_humantime_duration("1d").unwrap(),
            std::time::Duration::from_secs(86_400),
        );
    }

    #[test]
    fn parse_humantime_duration_rejects_empty_input() {
        let err = parse_humantime_duration("").unwrap_err();
        assert!(err.contains("empty"), "{err}");
        let err = parse_humantime_duration(" ").unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn parse_humantime_duration_rejects_missing_digits() {
        let err = parse_humantime_duration("s").unwrap_err();
        assert!(err.contains("ASCII digits"), "{err}");
        let err = parse_humantime_duration("min").unwrap_err();
        assert!(err.contains("ASCII digits"), "{err}");
    }

    #[test]
    fn parse_humantime_duration_rejects_unknown_unit() {
        let err = parse_humantime_duration("30y").unwrap_err();
        assert!(err.contains("unknown unit"), "{err}");
        // Mixed-unit forms are rejected (V1 is single-unit only).
        let err = parse_humantime_duration("1h30m").unwrap_err();
        assert!(err.contains("unknown unit"), "{err}");
    }

    #[test]
    fn parse_humantime_duration_rejects_trailing_junk() {
        // Garbage after the unit fails the unit-match arm.
        let err = parse_humantime_duration("30sX").unwrap_err();
        assert!(
            err.contains("unknown unit") || err.contains("expected"),
            "{err}"
        );
        // Whitespace between digits and unit is also rejected — the
        // unit slice begins right after the trailing digit.
        let err = parse_humantime_duration("30 s").unwrap_err();
        assert!(
            err.contains("unknown unit") || err.contains("expected"),
            "{err}"
        );
    }

    #[test]
    fn parse_humantime_duration_handles_outer_whitespace() {
        // Outer whitespace is trimmed so `--poll-interval " 30s "`
        // (e.g. from a quoted shell var) round-trips.
        assert_eq!(
            parse_humantime_duration(" 30s ").unwrap(),
            std::time::Duration::from_secs(30),
        );
    }

    #[test]
    fn hostname_for_claim_falls_back_to_unknown() {
        let _env_guard = EnvGuard::new();
        unsafe { std::env::remove_var("HOSTNAME") };
        assert_eq!(hostname_for_claim(), "unknown");
        unsafe { std::env::set_var("HOSTNAME", "ci-runner-7") };
        assert_eq!(hostname_for_claim(), "ci-runner-7");
    }

    #[test]
    fn env_guard_restores_prior_values() {
        let env_guard = EnvGuard::new();
        let expected_hostname = env_guard.prior_hostname.clone();
        let expected_djogi_env = env_guard.prior_djogi_env.clone();
        let next_hostname = if expected_hostname.as_deref() == Some("ci-runner-7") {
            "ci-runner-8"
        } else {
            "ci-runner-7"
        };
        let next_djogi_env = if expected_djogi_env.as_deref() == Some("staging") {
            "production"
        } else {
            "staging"
        };
        unsafe { std::env::set_var("HOSTNAME", next_hostname) };
        unsafe { std::env::set_var("DJOGI_ENV", next_djogi_env) };
        drop(env_guard);
        assert_eq!(std::env::var("HOSTNAME").ok(), expected_hostname);
        assert_eq!(std::env::var("DJOGI_ENV").ok(), expected_djogi_env);
    }

    // ── helpers ──────────────────────────────────────────────────────

    #[test]
    fn justify_is_empty_handles_none_and_blank() {
        assert!(justify_is_empty(None));
        assert!(justify_is_empty(Some("")));
        assert!(justify_is_empty(Some(" ")));
        assert!(!justify_is_empty(Some("real reason")));
    }

    #[test]
    fn require_justify_for_destructive_refuses_without_reason() {
        let err = require_justify_for_destructive(true, None).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        let err = require_justify_for_destructive(true, Some(" ")).unwrap_err();
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
    fn require_destructive_gate_passes_for_non_destructive_plan() {
        use djogi::live_migrate::{
            LivePlan, PlanClassification, PlanHeader, Step, StepKind, StepParameters,
        };
        let plan = LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "demo".to_string(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428000000__demo".to_string(),
                target_database: "main".to_string(),
                app_label: "".to_string(),
            },
            steps: vec![Step {
                kind: StepKind::ExpandSchema,
                ordinal: 0,
                parameters: StepParameters::ExpandSchema {
                    sql_segments: vec!["ALTER TABLE foo ADD COLUMN bar INT".to_string()],
                },
            }],
        };
        // Without --allow-destructive: passes (no destructive step).
        require_destructive_gate_for_plan(&plan, false, None).unwrap();
    }

    #[test]
    fn require_destructive_gate_refuses_destructive_plan_without_flag() {
        use djogi::live_migrate::{
            LivePlan, PlanClassification, PlanHeader, Step, StepKind, StepParameters,
        };
        let plan = LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "demo".to_string(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428000000__demo".to_string(),
                target_database: "main".to_string(),
                app_label: "".to_string(),
            },
            steps: vec![Step {
                kind: StepKind::CleanupLegacyState,
                ordinal: 0,
                parameters: StepParameters::CleanupLegacyState {
                    sql_segments: vec!["ALTER TABLE foo DROP COLUMN baz".to_string()],
                },
            }],
        };
        // No `--allow-destructive` → refuse.
        let err = require_destructive_gate_for_plan(&plan, false, None).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        // `--allow-destructive` without `--justify` → refuse.
        let err = require_destructive_gate_for_plan(&plan, true, None).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        let err = require_destructive_gate_for_plan(&plan, true, Some(" ")).unwrap_err();
        assert!(matches!(err, LiveCmdError::ArgRefused(_)));
        // Both set → accepts.
        require_destructive_gate_for_plan(&plan, true, Some("ops runbook RB-19")).unwrap();
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
    fn assert_run_status_accepts_pending_running() {
        assert!(assert_run_status_allows_progress(PlanStatus::Pending).is_ok());
        assert!(assert_run_status_allows_progress(PlanStatus::Running).is_ok());
    }

    #[test]
    fn assert_run_status_refuses_paused_pointing_to_resume() {
        // `Paused` is the operator's explicit checkpoint state — `live run`
        // does not auto-advance through it; the operator must invoke
        // `live resume` to re-enter the run loop. This pins the gate.
        let err = assert_run_status_allows_progress(PlanStatus::Paused)
            .expect_err("paused must be a state conflict for `live run`");
        match err {
            LiveCmdError::StateConflict(msg) => {
                assert!(msg.contains("paused"), "{msg}");
                assert!(msg.contains("live resume"), "{msg}");
            }
            other => panic!("expected StateConflict, got {other:?}"),
        }
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
    fn assert_abandon_status_refuses_every_terminal_state() {
        // All three terminal states refuse: Complete, Abandoned, Failed.
        for status in [
            PlanStatus::Complete,
            PlanStatus::Abandoned,
            PlanStatus::Failed,
        ] {
            let err = assert_abandon_status(status)
                .expect_err("terminal status must be a state conflict for abandon");
            assert!(matches!(err, LiveCmdError::StateConflict(_)));
        }
        // Every non-terminal status is a valid abandonment target.
        for status in [
            PlanStatus::Pending,
            PlanStatus::Running,
            PlanStatus::Paused,
            PlanStatus::Validating,
            PlanStatus::Cutover,
            PlanStatus::Finalizing,
        ] {
            assert!(assert_abandon_status(status).is_ok(), "{status:?} accepts");
        }
    }

    #[test]
    fn assert_abandon_status_failed_message_points_to_fresh_plan() {
        // The Failed-refusal message is the operator's signpost — pin
        // the wording so refactors notice if the audit trail story drifts.
        let err = assert_abandon_status(PlanStatus::Failed).expect_err("failed must refuse");
        match err {
            LiveCmdError::StateConflict(msg) => {
                assert!(msg.contains("failed"), "{msg}");
                assert!(msg.contains("fresh plan") || msg.contains("audit"), "{msg}",);
            }
            other => panic!("expected StateConflict, got {other:?}"),
        }
    }

    // ── env gate ─────────────────────────────────────────────────────

    #[test]
    fn force_allowed_when_djogi_env_unset() {
        let _env_guard = EnvGuard::new();
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
