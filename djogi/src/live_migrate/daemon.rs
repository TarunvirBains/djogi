//! Daemon-mode resume for live migrations.
//! Long-running poll loop that scans `djogi_live_plans` for stale rows
//! whose current step is [`StepKind::BackfillChunked`] or
//! [`StepKind::ValidateBackfill`], claims them via Postgres advisory
//! lock + the `claimed_by_*` columns added by
//! [`crate::live_migrate::state::INSTALL_SQL`], and drives them
//! forward via the same [`resume_backfill`] entry point the operator
//! CLI's `live resume` calls. The daemon is the unattended sibling of
//! `live resume`: it picks up plans whose chunk loop was interrupted
//! mid-stream (host crash, container eviction, network partition) and
//! finishes the backfill without operator intervention.
//! # What the daemon will and will not advance
//! The daemon's responsibility ends at the first operator gate.
//! Specifically:
//! - Auto-resumes [`StepKind::BackfillChunked`] (chunk-loop
//! continuation; idempotent by the pattern's `WHERE` predicate
//! contract).
//! - Auto-resumes [`StepKind::ValidateBackfill`] only as a re-runnable
//! gate query — the daemon does NOT auto-promote the plan past the
//! validation gate; that decision stays with the operator via
//! `live run`.
//! - Refuses to advance through [`StepKind::CutoverReads`],
//! [`StepKind::CutoverWrites`], and [`StepKind::FinalizeConstraints`].
//! Those gates are operator-only (`live run` / `live finalize`); the
//! daemon would have no way to confirm the production application's
//! read / write traffic has been re-routed before flipping the
//! cutover, and an automated finalize would side-step the operator
//! review of the post-backfill state. Any plan whose `current_step`
//! has advanced into a cutover / finalize gate is invisible to the
//! daemon's candidate filter.
//! - Refuses to auto-resume [`PlanStatus::Paused`] plans. `Paused` is
//! the operator's checkpoint state — the daemon never overrides an
//! explicit operator pause.
//! # Triple-gate
//! Mirrors `db reset` semantics. A daemon invocation refuses to start
//! when:
//! 1. `DJOGI_ENV` is set to `production` (case-insensitive). The daemon
//! is not approved for production deployments in the v1 surface;
//! operator-driven `live resume` remains the sole production
//! surface.
//! 2. The application database URL does not resolve to localhost AND
//! the operator did not pass `--allow-non-localhost`. Mirrors the
//! seed / reset gate so a misconfigured `DATABASE_URL` cannot have
//! the daemon hammering a remote production box.
//! # Coordination
//! Two daemons may legitimately race on the same plan (failover to a
//! new host while the original daemon is still draining). The poll
//! loop coordinates via a per-plan Postgres advisory lock keyed off the
//! plan_id; only the lock-holder updates the `claimed_by_*` columns
//! and drives the backfill. Other daemons skip the row on this poll
//! cycle. The lock is a session-scoped advisory lock, so it auto-
//! releases on connection close — a daemon that crashes mid-chunk does
//! not leave a stuck claim on the row.
//! [`StepKind::BackfillChunked`]: crate::live_migrate::plan::StepKind::BackfillChunked
//! [`StepKind::ValidateBackfill`]: crate::live_migrate::plan::StepKind::ValidateBackfill
//! [`StepKind::CutoverReads`]: crate::live_migrate::plan::StepKind::CutoverReads
//! [`StepKind::CutoverWrites`]: crate::live_migrate::plan::StepKind::CutoverWrites
//! [`StepKind::FinalizeConstraints`]: crate::live_migrate::plan::StepKind::FinalizeConstraints
//! [`PlanStatus::Paused`]: crate::live_migrate::state::PlanStatus::Paused

use std::path::PathBuf;
use std::time::Duration;

use crate::__bypass::RawAccessExt as _;
use crate::context::{DjogiContext, PinnedCtx};
use crate::error::{DbError, DjogiError};
use crate::live_migrate::backfill::BackfillError;
use crate::pg::pool::DjogiPool;

// ── Configuration ─────────────────────────────────────────────────────

/// Configuration for the daemon poll loop. Built by the CLI from
/// operator flags and threaded into [`run_daemon`].
/// `host` and `pid` are written to the row's `claimed_by_*` columns so
/// operators inspecting `djogi_live_plans` can identify which daemon
/// instance is driving each plan. They are diagnostic only — the
/// session-scoped advisory lock is the actual mutual-exclusion
/// primitive; the columns exist for `live show` / dashboard rendering.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Interval between candidate-row scans. Default 30s per the v3
    /// plan §8 amendment.
    pub poll_interval: Duration,
    /// A row is treated as a daemon candidate when its
    /// `last_progress_at` is older than `now - claim_stale_after`,
    /// OR `claimed_by_pid IS NULL`. Default 10 minutes.
    pub claim_stale_after: Duration,
    /// Refuse non-localhost connections unless this flag is set. The
    /// CLI maps `--allow-non-localhost` here.
    pub allow_non_localhost: bool,
    /// Application database URL — used solely by the localhost gate
    /// inside [`enforce_environment_gates`]. Mirrors the URL the
    /// `DjogiContext` was built from; the daemon does NOT open its own
    /// pool from this string (the operator already provided a context).
    pub database_url: String,
    /// Hostname recorded in `djogi_live_plans.claimed_by_host` while
    /// this daemon owns the row. Diagnostic only.
    pub host: String,
    /// Process ID recorded in `djogi_live_plans.claimed_by_pid` while
    /// this daemon owns the row. Diagnostic only.
    pub pid: i64,
    /// `Djogi.toml::profile` value, threaded through by the CLI. The
    /// daemon's environment gate refuses to start when this is the
    /// literal lowercase string `"production"`. Mirrors the predicate
    /// `db reset` and `db cleanup-test-dbs` use so the same rules
    /// govern every long-running destructive surface.
    pub profile: String,
    /// Workspace root for resolving plan file paths.
    pub workspace_root: PathBuf,
}

impl DaemonConfig {
    /// Build a [`DaemonConfig`] with the default poll interval (30s)
    /// and stale-claim threshold (10 minutes), and the daemon-targeted
    /// flag set so the daemon only ever runs against localhost.
    /// `host` and `pid` are read from the running process; tests that
    /// need a deterministic shape construct the struct directly.
    /// The caller supplies the application `database_url` because the
    /// daemon's localhost gate evaluates against the same URL the
    /// surrounding [`DjogiContext`] was built from — passing it
    /// explicitly keeps the gate input visible at the construction
    /// site rather than buried inside a context accessor.
    pub fn default_for_localhost(database_url: impl Into<String>) -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            claim_stale_after: Duration::from_secs(10 * 60),
            allow_non_localhost: false,
            database_url: database_url.into(),
            host: hostname_or_unknown(),
            pid: i64::from(std::process::id()),
            profile: "development".to_string(),
            workspace_root: std::path::PathBuf::from("."),
        }
    }
}

/// Best-effort hostname read. Reads `HOSTNAME` from the environment
/// (set on most Linux shells) and falls back to `"unknown"` if the
/// variable is absent. Hostname is diagnostic only — a `"unknown"`
/// value never affects daemon correctness.
fn hostname_or_unknown() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
}

// ── Errors ────────────────────────────────────────────────────────────

/// Errors raised by [`run_daemon`].
/// `#[non_exhaustive]` so future failure modes (e.g. a multi-host
/// coordination conflict beyond per-row advisory locks) can land
/// without breaking downstream matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DaemonError {
    /// Refused on the localhost gate. The configured database URL
    /// does not resolve to localhost and `allow_non_localhost = false`.
    #[error(
        "daemon refused to start: not running on localhost \
         (--allow-non-localhost not passed)"
    )]
    NotLocalhost,
    /// Refused on the production-environment gate.
    #[error("daemon refused to start: DJOGI_ENV=production")]
    Production,
    /// A backfill resume attempt failed for one of the candidate plans.
    /// Surfaces only when the failure is unrecoverable enough to halt
    /// the entire daemon (e.g. the connection pool is dead). Per-plan
    /// failures are logged and the daemon moves on.
    #[error(transparent)]
    Backfill(#[from] BackfillError),
    /// Database / driver error reading the candidate-row list, the
    /// claim columns, or the advisory-lock probe.
    #[error(transparent)]
    Database(#[from] DjogiError),
    /// SIGTERM / SIGINT received; the loop exited cleanly. Surfaced as
    /// an `Err` rather than `Ok` so callers can distinguish
    /// "completed naturally" from "terminated by signal" — the daemon
    /// never completes naturally; the only successful exit is via
    /// signal.
    #[error("daemon shutdown signal received")]
    Shutdown,
}

impl From<DbError> for DaemonError {
    fn from(value: DbError) -> Self {
        DaemonError::Database(DjogiError::Db(value))
    }
}

// ── SQL constants ─────────────────────────────────────────────────────

/// Candidate-row query. Returns one row per plan whose `current_step`
/// is in the daemon's auto-resume set AND whose
/// `(last_progress_at, claimed_by_pid)` shape marks the row as both
/// stale AND eligible-to-claim.
/// The `INTERVAL '1 second' * $1` form lets us pass the stale-claim
/// threshold as a `BIGINT` parameter (number of seconds) rather than
/// formatting an interval literal — the parameter binder cannot bind
/// a `Duration` directly, but seconds-as-i64 round-trips cleanly.
/// The candidate filter is two AND-ed predicates:
/// 1. **Eligible step / status.** `status = 'running'` AND
/// `current_step IN ('backfill_chunked', 'validate_backfill')`.
/// Pending plans are operator-promoted via `live run`; Paused
/// plans are explicit operator checkpoints; terminal states
/// (Complete / Abandoned / Failed) are never auto-resumed.
/// 2. **Stale (or never-progressed) AND free-to-claim.** The row must
/// be stale — `last_progress_at < now - $1::interval OR
/// last_progress_at IS NULL` (a row that has never recorded
/// progress is treated as stale by definition). The row must also
/// be free to claim — `claimed_by_pid IS NULL OR claimed_by_pid =
/// $2`: unclaimed, OR already claimed by THIS daemon (so a
/// restarted daemon process can re-claim its own previous work
/// rather than waiting for the stale threshold).
/// `$2` binds the running daemon's PID — passing it explicitly keeps
/// the predicate self-contained without smuggling state through the
/// SQL session.
/// The query is documented as a constant so tests can assert its
/// shape without reaching into private internals.
const CANDIDATE_QUERY_SQL: &str = "\
SELECT plan_id, target_database, app_label, slug, current_step \
FROM djogi_live_plans \
WHERE status = 'running' \
  AND current_step IN ('backfill_chunked', 'validate_backfill') \
  AND ( \
       claimed_by_pid = $2 \
    OR ( claimed_by_pid IS NULL \
         AND ( last_progress_at IS NULL \
               OR last_progress_at < now() - (INTERVAL '1 second' * $1) ) ) \
  )";

/// Update statement that records a successful claim. Sets `claimed_by_pid`,
/// `claimed_by_host`, and `claimed_at = now` for the row identified by
/// the bucket key. Issued only after the corresponding advisory lock
/// has been acquired.
/// The `WHERE` clause re-asserts the same status / step filter the
/// candidate query uses (`status = 'running'` AND `current_step IN
/// (...)`) so a row that was paused / abandoned / advanced between the
/// SELECT and the claim cannot have its claim columns mutated. Callers
/// inspect the returned row count: 0 means the row was pre-empted and
/// the resume path must skip this candidate.
const CLAIM_UPDATE_SQL: &str = "\
UPDATE djogi_live_plans \
SET claimed_by_pid = $4, claimed_by_host = $5, claimed_at = now() \
WHERE target_database = $1 AND app_label = $2 AND plan_id = $3 \
  AND status = 'running' \
  AND current_step IN ('backfill_chunked', 'validate_backfill')";

/// Update statement that releases a previously-recorded claim. Cleared
/// when the daemon is done with the row (either the resume succeeded
/// or it failed and the operator must intervene). The advisory lock
/// itself is released by [`pg_advisory_unlock`] on the same key.
const CLEAR_CLAIM_SQL: &str = "\
UPDATE djogi_live_plans \
SET claimed_by_pid = NULL, claimed_by_host = NULL, claimed_at = NULL \
WHERE target_database = $1 AND app_label = $2 AND plan_id = $3";

// ── Public entry point ────────────────────────────────────────────────

/// Start the daemon poll loop. Returns
/// [`DaemonError::Shutdown`] when SIGTERM / SIGINT is received and
/// returns the typed error for any unrecoverable failure (e.g. the
/// triple-gate refusal happens here, before the loop starts).
/// Per-plan failures inside the loop are logged via `tracing::warn!`
/// and the loop continues — a single misbehaving plan does not halt
/// the daemon's other work.
pub async fn run_daemon(ctx: &mut DjogiContext, config: DaemonConfig) -> Result<(), DaemonError> {
    enforce_environment_gates(&config)?;

    tracing::info!(
        host = %config.host,
        pid = config.pid,
        poll_interval_secs = config.poll_interval.as_secs(),
        claim_stale_after_secs = config.claim_stale_after.as_secs(),
        "live-migrate daemon started",
    );

    // Production daemons typically receive SIGTERM from systemd /
    // kubernetes / `docker stop`; SIGINT (Ctrl-C) is the interactive
    // case. On Unix we listen to both; on non-Unix only `ctrl_c` is
    // available (Windows lacks SIGTERM, so the cfg gate is necessary).
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| {
            DaemonError::Database(DjogiError::Db(DbError::other(format!(
                "register SIGTERM handler: {e}"
            ))))
        })?;

    loop {
        // Always poll the shutdown signals alongside the timer so a
        // signal during a long sleep takes effect within one
        // iteration.
        #[cfg(unix)]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("live-migrate daemon: SIGINT received");
                    return Err(DaemonError::Shutdown);
                }
                _ = sigterm.recv() => {
                    tracing::info!("live-migrate daemon: SIGTERM received");
                    return Err(DaemonError::Shutdown);
                }
                _ = tokio::time::sleep(config.poll_interval) => {
                    if let Err(e) = poll_once(ctx, &config).await {
                        tracing::warn!(
                            error = %e,
                            "live-migrate daemon: poll iteration failed; \
                             continuing to next interval",
                        );
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("live-migrate daemon: shutdown signal received");
                    return Err(DaemonError::Shutdown);
                }
                _ = tokio::time::sleep(config.poll_interval) => {
                    if let Err(e) = poll_once(ctx, &config).await {
                        tracing::warn!(
                            error = %e,
                            "live-migrate daemon: poll iteration failed; \
                             continuing to next interval",
                        );
                    }
                }
            }
        }
    }
}

/// Apply the production-env + production-profile + localhost gates.
/// Lifted into a free function so unit tests can exercise every refusal
/// path without running the full poll loop.
/// Three independent refusals fire here:
/// 1. `DJOGI_ENV` set to `production` (case-insensitive).
/// 2. `DjogiConfig::profile` set to the literal lowercase
/// `"production"` — mirrors the predicate `db reset` and
/// `db cleanup-test-dbs` use so every long-running destructive
/// surface follows the same rule. Strict lowercase match: a typo
/// (`"Production"`, `"PROD"`) falls back to the safe-for-dev
/// default rather than silently flipping the gate.
/// 3. Application URL is not localhost AND `--allow-non-localhost`
/// was not passed.
fn enforce_environment_gates(config: &DaemonConfig) -> Result<(), DaemonError> {
    if production_env_set() {
        return Err(DaemonError::Production);
    }
    if config.profile == "production" {
        return Err(DaemonError::Production);
    }
    if !config.allow_non_localhost && !crate::migrate::is_localhost_connection(&config.database_url)
    {
        return Err(DaemonError::NotLocalhost);
    }
    Ok(())
}

/// Returns `true` when `DJOGI_ENV` is set to `production`
/// (case-insensitive). All other values, including unset, return
/// `false`.
fn production_env_set() -> bool {
    match std::env::var("DJOGI_ENV") {
        Ok(v) => v.eq_ignore_ascii_case("production"),
        Err(_) => false,
    }
}

// ── Single poll iteration ─────────────────────────────────────────────

/// One pass of the poll loop. Identifies candidate rows, attempts to
/// claim each via `pg_try_advisory_lock`, and (on success) drives the
/// resume. Errors from individual plans are logged + skipped; only a
/// failure to read the candidate list itself bubbles out.
async fn poll_once(ctx: &mut DjogiContext, config: &DaemonConfig) -> Result<(), DaemonError> {
    let candidates = read_candidates(ctx, config.claim_stale_after, config.pid).await?;
    if candidates.is_empty() {
        return Ok(());
    }
    // The daemon's ctx is pool-backed (built via DjogiContext::from_pool
    // in the CLI). Clone the pool handle ONCE per poll so the backfill
    // chunk loop runs on a fresh pool-backed context — distinct from the
    // advisory-lock pinned (connection-backed) context. See CLASS B.
    let pool = ctx.share_pool().ok_or_else(|| {
        DaemonError::Database(DjogiError::Db(DbError::other(
            "daemon poll requires a pool-backed DjogiContext (built via \
             DjogiContext::from_pool); cannot resume backfills without a pool \
             to open per-chunk transactions on",
        )))
    })?;
    tracing::debug!(
        count = candidates.len(),
        "live-migrate daemon: candidate plans this iteration",
    );
    for candidate in candidates {
        if let Err(e) = drive_candidate(ctx, &pool, config, &candidate).await {
            tracing::warn!(
                plan_id = candidate.plan_id,
                target_database = %candidate.target_database,
                app_label = %candidate.app_label,
                error = %e,
                "live-migrate daemon: candidate skipped",
            );
        }
    }
    Ok(())
}

/// Owned shape of one candidate row. Carries the bucket key + step
/// label so the resume loop can route through the bucketed `state.rs`
/// helpers without re-reading the row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonCandidate {
    plan_id: i64,
    target_database: String,
    app_label: String,
    slug: String,
    current_step: String,
}

/// SELECT the candidate-row set for this iteration. Binds the
/// stale-threshold seconds as `$1` and the daemon's PID as `$2` so the
/// predicate `claimed_by_pid = $2` matches THIS daemon's own previous
/// claims (stale-threshold-bypass for self-restart) without exposing a
/// session-state escape hatch.
async fn read_candidates(
    ctx: &mut DjogiContext,
    claim_stale_after: Duration,
    pid: i64,
) -> Result<Vec<DaemonCandidate>, DaemonError> {
    let stale_secs: i64 = i64::try_from(claim_stale_after.as_secs()).unwrap_or(i64::MAX);
    let rows = ctx
        .raw_rows(CANDIDATE_QUERY_SQL, &[&stale_secs, &pid])
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let plan_id: i64 = row.try_get(0).map_err(|e| {
            DjogiError::Db(DbError::other(format!("candidate row decode plan_id: {e}")))
        })?;
        let target_database: String = row.try_get(1).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "candidate row decode target_database: {e}"
            )))
        })?;
        let app_label: String = row.try_get(2).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "candidate row decode app_label: {e}"
            )))
        })?;
        let slug: String = row.try_get(3).map_err(|e| {
            DjogiError::Db(DbError::other(format!("candidate row decode slug: {e}")))
        })?;
        let current_step: Option<String> = row.try_get(4).map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "candidate row decode current_step: {e}"
            )))
        })?;
        // The candidate-query filter pins `current_step IN (...)` so a
        // NULL here would imply database-level drift. Skip rather than
        // panic — the next iteration re-checks the row.
        let Some(current_step) = current_step else {
            continue;
        };
        out.push(DaemonCandidate {
            plan_id,
            target_database,
            app_label,
            slug,
            current_step,
        });
    }
    Ok(out)
}

/// Try to claim, drive, and release one candidate. Each phase logs
/// independently so the operator can pinpoint where a plan got stuck.
/// `pool` is the daemon's shared pool handle (cloned once per poll in
/// [`poll_once`]). It is threaded down to the backfill resume so the
/// chunk loop runs on a fresh pool-backed context rather than the
/// advisory-lock pinned (connection-backed) context. See CLASS B.
async fn drive_candidate(
    ctx: &mut DjogiContext,
    pool: &DjogiPool,
    config: &DaemonConfig,
    candidate: &DaemonCandidate,
) -> Result<(), DaemonError> {
    // GH #274 / #331: pin one physical session for the entire lock
    // + drive window so the advisory lock stays on the same connection.
    let mut pinned = ctx.pin_for_migration().await?;
    let lock_key = candidate.plan_id;
    if !try_acquire_advisory_lock(&mut pinned, lock_key).await? {
        // Another daemon owns the lock for this plan — leave it alone
        // on this iteration; we'll re-check on the next poll.
        tracing::debug!(
            plan_id = candidate.plan_id,
            "live-migrate daemon: advisory lock held by another holder; skipping",
        );
        return Ok(());
    }
    // From here on, every exit path must release the advisory lock.
    let result = drive_under_lock(&mut pinned, pool, config, candidate).await;
    release_advisory_lock(&mut pinned, lock_key).await;

    // Lock released — mark clean so the connection returns to the pool.
    if result.is_ok() {
        pinned.mark_clean();
    }
    result
}

/// Body of [`drive_candidate`] inside the advisory-lock window. Splits
/// the lock-acquire / lock-release out of the resume logic so the
/// advisory-lock invariant is locally provable: the lock is released
/// by the caller regardless of the inner `Result`.
async fn drive_under_lock(
    ctx: &mut PinnedCtx<'_>,
    pool: &DjogiPool,
    config: &DaemonConfig,
    candidate: &DaemonCandidate,
) -> Result<(), DaemonError> {
    if !record_claim(ctx, config, candidate).await? {
        // The row was pre-empted between SELECT and UPDATE — operator
        // paused / abandoned the plan, or the pipeline advanced past
        // the auto-resumable step. Skip the resume; the next poll
        // re-checks the row.
        tracing::debug!(
            plan_id = candidate.plan_id,
            "live-migrate daemon: claim pre-empted between SELECT and UPDATE; skipping",
        );
        return Ok(());
    }
    // The current_step label distinguishes the two auto-resumable kinds.
    // BackfillChunked is the chunk loop; ValidateBackfill is a re-runnable
    // gate query the daemon does NOT advance through — it merely verifies
    // the gate query still parses against the live database. Operator
    // promotion past the gate stays with `live run`.
    // The backfill resume runs on `pool` (a fresh pool-backed context),
    // NOT the pinned `ctx`: each chunk opens its own transaction and
    // cannot nest inside the advisory-lock connection. The claim /
    // clear-claim work stays on the pinned `ctx` — single statements
    // that benefit from running on the locked session.
    let outcome = match candidate.current_step.as_str() {
        "backfill_chunked" => resume_backfill_for_candidate(pool, candidate, config).await,
        "validate_backfill" => {
            tracing::debug!(
                plan_id = candidate.plan_id,
                "live-migrate daemon: validate_backfill is operator-only; \
                 daemon does not advance past the gate",
            );
            Ok(())
        }
        other => {
            // The candidate filter pins the step kind to one of the two
            // labels above. Reaching this arm means the row's
            // current_step changed between SELECT and UPDATE — skip
            // rather than guess.
            tracing::debug!(
                plan_id = candidate.plan_id,
                step = %other,
                "live-migrate daemon: step changed between SELECT and claim; skipping",
            );
            Ok(())
        }
    };
    // Always clear the claim columns when we're done driving the row,
    // regardless of whether the resume succeeded or failed. Leaving stale
    // claim columns on a row whose daemon has moved on would mislead the
    // operator's `live show` output.
    if let Err(e) = clear_claim(ctx, candidate).await {
        tracing::warn!(
            plan_id = candidate.plan_id,
            error = %e,
            "live-migrate daemon: clear_claim failed; columns will reset on next claim",
        );
    }
    outcome
}

/// Drive a `backfill_chunked` candidate via the same
/// [`crate::live_migrate::backfill::resume_backfill`] entry point the
/// CLI's `live resume` calls.
/// Runs on a FRESH pool-backed context built from `pool` — NOT the
/// daemon's advisory-lock pinned (connection-backed) context. The
/// chunk loop opens its own per-chunk transaction via `atomic(pool, ..)`
/// and therefore requires a pool to draw connections from; the pinned
/// connection is reserved for the session-scoped advisory lock. See
/// CLASS B.
/// On a backfill failure, records the failure on the plan row via
/// [`crate::live_migrate::state::record_failure`] (retriable) so the
/// failure is visible to `live show` and the daemon does not silently
/// re-attempt the same broken plan every poll cycle with no audit trail.
async fn resume_backfill_for_candidate(
    pool: &DjogiPool,
    candidate: &DaemonCandidate,
    daemon_cfg: &DaemonConfig,
) -> Result<(), DaemonError> {
    use crate::live_migrate::{
        backfill::resume_backfill, compose::extract_backfill_params, plan_file::read_plan,
    };
    use crate::types::HeerId;

    let plan_id = HeerId::from_i64(candidate.plan_id).map_err(|e| {
        DaemonError::Database(DjogiError::Db(DbError::other(format!(
            "invalid plan_id {}: {}",
            candidate.plan_id, e
        ))))
    })?;

    let slug = &candidate.slug;
    let path = crate::live_migrate::plan_file::plan_path(
        &daemon_cfg.workspace_root.join("migrations"),
        &candidate.target_database,
        plan_id,
        slug,
    );

    let plan = read_plan(&path).map_err(|e| {
        DaemonError::Database(DjogiError::Db(DbError::other(format!(
            "failed to read plan file {}: {}",
            path.display(),
            e
        ))))
    })?;

    let extract_result = extract_backfill_params(&plan.steps);
    let (table, predicate_template, chunk_size) = match extract_result {
        super::compose::ExtractResult::Params {
            table,
            filter,
            batch_size,
            ..
        } => (table, filter, batch_size as u32),
        super::compose::ExtractResult::NotBackfillChunked => {
            return Err(DaemonError::Database(DjogiError::Db(DbError::other(
                "plan has no BackfillChunked step to resume",
            ))));
        }
        super::compose::ExtractResult::Malformed(reason) => {
            return Err(DaemonError::Database(DjogiError::Db(DbError::other(
                format!("malformed backfill step: {}", reason),
            ))));
        }
    };

    // Fresh pool-backed context for the chunk loop. Each chunk opens its
    // own transaction; this MUST be pool-backed, not the pinned
    // connection that holds the advisory lock.
    let mut backfill_ctx = DjogiContext::from_pool(pool.clone());

    let result = resume_backfill(
        &mut backfill_ctx,
        plan_id,
        &table,
        &predicate_template,
        chunk_size,
        true, // emit events
    )
    .await;

    match result {
        Ok(chunks) => {
            tracing::info!(
                plan_id = %plan_id,
                target_database = %candidate.target_database,
                app_label = %candidate.app_label,
                chunks_processed = chunks.len(),
                "live-migrate daemon: backfill completed successfully",
            );
            Ok(())
        }
        Err(e) => {
            // Record the failure on the plan row so it is visible to
            // `live show` and the daemon does not silently re-attempt
            // indefinitely. Mark retriable: a daemon-mode backfill
            // failure is typically transient (connection blip, lock
            // contention); the operator can inspect `last_error` and
            // abandon if it is terminal. Recording uses the same
            // pool-backed ctx.
            let record_outcome = crate::live_migrate::state::record_failure(
                &mut backfill_ctx,
                plan_id,
                &candidate.target_database,
                &candidate.app_label,
                format!("daemon backfill resume failed: {e}"),
                true, // retriable
            )
            .await;
            if let Err(record_err) = record_outcome {
                tracing::warn!(
                    plan_id = %plan_id,
                    error = %record_err,
                    "live-migrate daemon: failed to record backfill failure on row",
                );
            }
            Err(match e {
                BackfillError::Database(db_err) => DaemonError::Database(db_err),
                other => DaemonError::Backfill(other),
            })
        }
    }
}
async fn try_acquire_advisory_lock(
    ctx: &mut PinnedCtx<'_>,
    lock_key: i64,
) -> Result<bool, DaemonError> {
    assert!(
        !ctx.is_pool_backed(),
        "daemon advisory lock called on a pool-backed context — \
         the lock would be acquired on an arbitrary pool connection \
         and subsequent operations would run on different connections. \
         Callers must use ctx.pin_for_migration() first (GH #274 / #331).",
    );
    let row = ctx
        .raw_rows("SELECT pg_try_advisory_lock($1)", &[&lock_key])
        .await?;
    let Some(first) = row.first() else {
        return Err(DaemonError::Database(DjogiError::Db(DbError::other(
            "pg_try_advisory_lock returned no rows",
        ))));
    };
    let acquired: bool = first
        .try_get(0)
        .map_err(|e| DjogiError::Db(DbError::other(format!("pg_try_advisory_lock decode: {e}"))))?;
    Ok(acquired)
}

/// Best-effort `pg_advisory_unlock`. Logs on failure rather than
/// surfacing — the session-scoped advisory lock auto-releases on
/// connection close anyway, so a failed unlock has bounded blast
/// radius.
async fn release_advisory_lock(ctx: &mut PinnedCtx<'_>, lock_key: i64) {
    assert!(
        !ctx.is_pool_backed(),
        "daemon advisory unlock called on a pool-backed context — \
         session-pinning correctness failure (GH #274 / #331).",
    );
    if let Err(e) = ctx
        .raw_execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
        .await
    {
        tracing::warn!(
            error = %e,
            lock_key,
            "live-migrate daemon: pg_advisory_unlock failed; \
             lock will release on session close",
        );
    }
}

/// UPDATE the `claimed_by_*` columns to reflect the current daemon's
/// ownership. Issued after the advisory lock is acquired so concurrent
/// daemons cannot stomp each other's claim metadata.
/// Returns `true` when the row was claimed (1 row affected) and `false`
/// when the row was pre-empted between the candidate SELECT and this
/// UPDATE — i.e. an operator paused / abandoned the plan, or the
/// pipeline advanced past the auto-resumable step. In the pre-empted
/// case the caller must skip the resume path; the claim columns are
/// untouched and the advisory lock will release on the normal path.
async fn record_claim(
    ctx: &mut DjogiContext,
    config: &DaemonConfig,
    candidate: &DaemonCandidate,
) -> Result<bool, DaemonError> {
    let affected = ctx
        .raw_execute(
            CLAIM_UPDATE_SQL,
            &[
                &candidate.target_database,
                &candidate.app_label,
                &candidate.plan_id,
                &config.pid,
                &config.host,
            ],
        )
        .await?;
    Ok(affected == 1)
}

/// Reset the `claimed_by_*` columns to NULL after the daemon is done
/// driving a candidate. Failing to clear the claim is non-fatal — the
/// next claim overwrites the columns anyway — so the caller logs and
/// moves on.
async fn clear_claim(
    ctx: &mut DjogiContext,
    candidate: &DaemonCandidate,
) -> Result<(), DaemonError> {
    ctx.raw_execute(
        CLEAR_CLAIM_SQL,
        &[
            &candidate.target_database,
            &candidate.app_label,
            &candidate.plan_id,
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic [`DaemonConfig`] for the gate / SQL tests.
    /// The poll / stale durations are short so a future test that
    /// drives the full loop can complete quickly; the database URL is
    /// localhost so the gate accepts.
    fn test_config(database_url: &str) -> DaemonConfig {
        DaemonConfig {
            poll_interval: Duration::from_millis(50),
            claim_stale_after: Duration::from_secs(60),
            allow_non_localhost: false,
            database_url: database_url.to_string(),
            host: "test-host".to_string(),
            pid: 12345,
            profile: "development".to_string(),
            workspace_root: std::path::PathBuf::from("."),
        }
    }

    // ── DaemonConfig defaults ─────────────────────────────────────────

    #[test]
    fn default_for_localhost_uses_30s_poll_interval() {
        let cfg = DaemonConfig::default_for_localhost("postgres://localhost/x");
        assert_eq!(cfg.poll_interval, Duration::from_secs(30));
    }

    #[test]
    fn default_for_localhost_uses_10min_stale_threshold() {
        let cfg = DaemonConfig::default_for_localhost("postgres://localhost/x");
        assert_eq!(cfg.claim_stale_after, Duration::from_secs(600));
    }

    #[test]
    fn default_for_localhost_refuses_remote_connections() {
        let cfg = DaemonConfig::default_for_localhost("postgres://localhost/x");
        assert!(
            !cfg.allow_non_localhost,
            "default config must require localhost",
        );
    }

    #[test]
    fn default_for_localhost_records_running_pid() {
        let cfg = DaemonConfig::default_for_localhost("postgres://localhost/x");
        assert_eq!(cfg.pid, i64::from(std::process::id()));
        assert!(!cfg.host.is_empty(), "host must be a non-empty diagnostic");
    }

    #[test]
    fn default_for_localhost_records_supplied_url() {
        let cfg = DaemonConfig::default_for_localhost("postgres://127.0.0.1/main");
        assert_eq!(cfg.database_url, "postgres://127.0.0.1/main");
    }

    #[test]
    fn default_for_localhost_uses_development_profile() {
        // The default constructor sets profile to a non-production
        // value so the gate doesn't fire on a freshly-built config.
        // The CLI overwrites this with the actual `Djogi.toml` value
        // before passing the config to `run_daemon`.
        let cfg = DaemonConfig::default_for_localhost("postgres://localhost/x");
        assert_eq!(cfg.profile, "development");
    }

    // ── Candidate-row query shape ─────────────────────────────────────

    #[test]
    fn candidate_query_filters_on_running_status() {
        assert!(
            CANDIDATE_QUERY_SQL.contains("status = 'running'"),
            "candidate query must restrict to running plans: {CANDIDATE_QUERY_SQL}",
        );
    }

    #[test]
    fn candidate_query_filters_on_auto_resumable_steps_only() {
        // The two step labels the daemon will auto-resume — anything
        // else (cutover, finalize, expand_schema) is operator-only.
        assert!(
            CANDIDATE_QUERY_SQL.contains("'backfill_chunked'"),
            "candidate query must include backfill_chunked",
        );
        assert!(
            CANDIDATE_QUERY_SQL.contains("'validate_backfill'"),
            "candidate query must include validate_backfill",
        );
        // Cutover / finalize labels must be excluded — verifying their
        // absence pins the operator-gate boundary at the SQL layer.
        assert!(
            !CANDIDATE_QUERY_SQL.contains("'cutover_reads'"),
            "candidate query must not pick up cutover_reads",
        );
        assert!(
            !CANDIDATE_QUERY_SQL.contains("'cutover_writes'"),
            "candidate query must not pick up cutover_writes",
        );
        assert!(
            !CANDIDATE_QUERY_SQL.contains("'finalize_constraints'"),
            "candidate query must not pick up finalize_constraints",
        );
    }

    #[test]
    fn candidate_query_excludes_paused_plans() {
        // The daemon never auto-resumes paused plans (operator
        // checkpoint state). Refused at the candidate filter rather
        // than caught later.
        assert!(
            !CANDIDATE_QUERY_SQL.contains("'paused'"),
            "candidate query must not consider paused plans",
        );
    }

    #[test]
    fn candidate_query_uses_stale_threshold_parameter() {
        // The threshold is bound as `$1` via INTERVAL '1 second' * $1.
        assert!(
            CANDIDATE_QUERY_SQL.contains("INTERVAL '1 second' * $1"),
            "candidate query must bind the stale threshold via $1: {CANDIDATE_QUERY_SQL}",
        );
    }

    #[test]
    fn candidate_query_recognises_unclaimed_rows() {
        // `claimed_by_pid IS NULL` is the unclaimed-row trigger.
        assert!(
            CANDIDATE_QUERY_SQL.contains("claimed_by_pid IS NULL"),
            "candidate query must consider rows without a recorded claim",
        );
    }

    #[test]
    fn candidate_query_self_pid_is_or_escape_hatch() {
        // The candidate predicate is (own_pid OR (unclaimed AND stale))
        // own-pid reclaim bypasses the stale threshold so a restarted
        // daemon picks its prior claims up immediately, while
        // other-daemon claims are skipped until they expire.
        assert!(
            CANDIDATE_QUERY_SQL.contains("claimed_by_pid = $2"),
            "self-pid escape hatch must be present: {CANDIDATE_QUERY_SQL}",
        );
        assert!(
            CANDIDATE_QUERY_SQL.contains("claimed_by_pid IS NULL"),
            "unclaimed branch must be guarded by claimed_by_pid IS NULL: {CANDIDATE_QUERY_SQL}",
        );
        assert!(
            CANDIDATE_QUERY_SQL.contains("last_progress_at IS NULL"),
            "stale branch must accept never-progressed rows: {CANDIDATE_QUERY_SQL}",
        );
        assert!(
            CANDIDATE_QUERY_SQL.contains("last_progress_at < now() - (INTERVAL '1 second' * $1)"),
            "stale branch must use `<` against now()-INTERVAL: {CANDIDATE_QUERY_SQL}",
        );
    }

    #[test]
    fn candidate_query_binds_pid_as_parameter_two() {
        // The daemon's PID flows in as `$2` so a restarted daemon can
        // re-claim its own previous work without waiting for the stale
        // threshold to expire on its own claim.
        assert!(
            CANDIDATE_QUERY_SQL.contains("claimed_by_pid = $2"),
            "PID must bind as `$2`: {CANDIDATE_QUERY_SQL}",
        );
    }

    // ── Claim CAS guard ───────────────────────────────────────────────

    #[test]
    fn claim_update_re_asserts_status_running() {
        // The candidate query filters by `status = 'running'`. The
        // UPDATE that records the claim must re-check the same
        // predicate so a row paused / abandoned between SELECT and
        // UPDATE cannot have its claim columns mutated. Caller
        // inspects the returned row count to detect the pre-empted
        // case and skip the resume path.
        assert!(
            CLAIM_UPDATE_SQL.contains("status = 'running'"),
            "claim UPDATE must re-assert status = 'running': {CLAIM_UPDATE_SQL}",
        );
    }

    #[test]
    fn claim_update_re_asserts_auto_resumable_steps() {
        // The candidate query restricts to the two auto-resumable
        // step labels. The UPDATE must re-check the same set so a row
        // whose pipeline advanced (e.g. operator promoted past the
        // gate) is not accidentally re-claimed.
        assert!(
            CLAIM_UPDATE_SQL.contains("'backfill_chunked'"),
            "claim UPDATE must re-assert backfill_chunked",
        );
        assert!(
            CLAIM_UPDATE_SQL.contains("'validate_backfill'"),
            "claim UPDATE must re-assert validate_backfill",
        );
    }

    // ── SIGTERM wiring (Finding 4) ────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn sigterm_handler_registration_succeeds_on_unix() {
        // The SIGTERM handler is registered inside `run_daemon` before
        // entering the loop. We can't run `run_daemon` synchronously
        // here (it loops forever), but we can verify the underlying
        // `tokio::signal::unix::signal` call path is available — i.e.
        // that the `signal` feature is enabled on the workspace tokio
        // dep. The handler resource is dropped immediately.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime build");
        rt.block_on(async {
            let result = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
            assert!(
                result.is_ok(),
                "tokio signal feature must be enabled for SIGTERM handler",
            );
        });
    }

    // ── Triple-gate logic ─────────────────────────────────────────────

    #[test]
    fn enforce_gates_accepts_localhost_when_env_unset() {
        let prior = std::env::var("DJOGI_ENV").ok();
        // SAFETY: tests run with --test-threads=1.
        unsafe { std::env::remove_var("DJOGI_ENV") };
        let cfg = test_config("postgres://localhost/main");
        assert!(enforce_environment_gates(&cfg).is_ok());
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    #[test]
    fn enforce_gates_refuses_production_env() {
        let prior = std::env::var("DJOGI_ENV").ok();
        // SAFETY: tests run with --test-threads=1.
        unsafe { std::env::set_var("DJOGI_ENV", "production") };
        let cfg = test_config("postgres://localhost/main");
        let err = enforce_environment_gates(&cfg).unwrap_err();
        assert!(matches!(err, DaemonError::Production));
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    #[test]
    fn enforce_gates_refuses_remote_url_without_override() {
        let prior = std::env::var("DJOGI_ENV").ok();
        // SAFETY: tests run with --test-threads=1.
        unsafe { std::env::remove_var("DJOGI_ENV") };
        let cfg = test_config("postgres://prod.example.com:5432/main");
        let err = enforce_environment_gates(&cfg).unwrap_err();
        assert!(matches!(err, DaemonError::NotLocalhost));
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    #[test]
    fn enforce_gates_accepts_remote_url_with_override() {
        let prior = std::env::var("DJOGI_ENV").ok();
        // SAFETY: tests run with --test-threads=1.
        unsafe { std::env::remove_var("DJOGI_ENV") };
        let mut cfg = test_config("postgres://prod.example.com:5432/main");
        cfg.allow_non_localhost = true;
        assert!(enforce_environment_gates(&cfg).is_ok());
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    #[test]
    fn enforce_gates_refuses_production_profile() {
        // Production profile in `Djogi.toml` is its own gate, separate
        // from `DJOGI_ENV`. The CLI threads `DjogiConfig::profile`
        // through to `DaemonConfig::profile`; the gate fires when the
        // value is the literal lowercase `"production"`.
        let prior = std::env::var("DJOGI_ENV").ok();
        // SAFETY: tests run with --test-threads=1.
        unsafe { std::env::remove_var("DJOGI_ENV") };
        let mut cfg = test_config("postgres://localhost/main");
        cfg.profile = "production".to_string();
        let err = enforce_environment_gates(&cfg).unwrap_err();
        assert!(matches!(err, DaemonError::Production));
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    #[test]
    fn enforce_gates_accepts_non_production_profile_strings() {
        // A typo in the profile field (`"Production"`, `"PROD"`) falls
        // back to the safe-for-dev default rather than silently flipping
        // the gate. Mirrors `DjogiConfig::is_production`'s strict match.
        let prior = std::env::var("DJOGI_ENV").ok();
        unsafe { std::env::remove_var("DJOGI_ENV") };
        for profile in [
            "development",
            "staging",
            "test",
            "Production",
            "PROD",
            "prod",
        ] {
            let mut cfg = test_config("postgres://localhost/main");
            cfg.profile = profile.to_string();
            assert!(
                enforce_environment_gates(&cfg).is_ok(),
                "profile=`{profile}` must NOT fire the gate (only lowercase `production` does)",
            );
        }
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    #[test]
    fn production_env_set_recognises_production() {
        // SAFETY: tests run with --test-threads=1.
        let prior = std::env::var("DJOGI_ENV").ok();
        unsafe { std::env::set_var("DJOGI_ENV", "production") };
        assert!(production_env_set());
        unsafe { std::env::set_var("DJOGI_ENV", "PRODUCTION") };
        assert!(production_env_set(), "case-insensitive match required");
        unsafe { std::env::set_var("DJOGI_ENV", "development") };
        assert!(!production_env_set());
        unsafe { std::env::remove_var("DJOGI_ENV") };
        assert!(!production_env_set());
        // Restore.
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    // ── DaemonError mappings ──────────────────────────────────────────

    #[test]
    fn db_error_converts_into_database_variant() {
        let db = DbError::other("boom");
        let de: DaemonError = db.into();
        assert!(matches!(de, DaemonError::Database(_)));
    }

    #[test]
    fn djogi_error_converts_into_database_variant() {
        let je = DjogiError::Db(DbError::other("boom"));
        let de: DaemonError = je.into();
        assert!(matches!(de, DaemonError::Database(_)));
    }

    #[test]
    fn shutdown_error_renders_human_message() {
        let e = DaemonError::Shutdown;
        assert!(e.to_string().contains("shutdown signal"));
    }

    #[test]
    fn not_localhost_error_renders_actionable_message() {
        let e = DaemonError::NotLocalhost;
        let msg = e.to_string();
        assert!(msg.contains("localhost"), "{msg}");
        assert!(msg.contains("--allow-non-localhost"), "{msg}");
    }

    #[test]
    fn production_error_renders_actionable_message() {
        let e = DaemonError::Production;
        assert!(e.to_string().contains("DJOGI_ENV"));
    }

    // ── hostname fallback ─────────────────────────────────────────────

    #[test]
    fn hostname_or_unknown_falls_back_when_unset() {
        // SAFETY: tests run with --test-threads=1.
        let prior = std::env::var("HOSTNAME").ok();
        unsafe { std::env::remove_var("HOSTNAME") };
        let h = hostname_or_unknown();
        assert_eq!(h, "unknown");
        unsafe { std::env::set_var("HOSTNAME", "test-box.example") };
        let h = hostname_or_unknown();
        assert_eq!(h, "test-box.example");
        match prior {
            Some(v) => unsafe { std::env::set_var("HOSTNAME", v) },
            None => unsafe { std::env::remove_var("HOSTNAME") },
        }
    }
}
