//! `db reset` orchestrator.
//! `db reset` is the destructive triple-gated path: it drops the
//! application database, recreates it, and replays every committed
//! migration found under `migrations/<database>/<app>/`. The triple
//! gate per the brief:
//! 1. `DATABASE_URL` MUST resolve to localhost (reused
//!    [`super::policy::is_localhost_connection`]).
//! 2. `Djogi.toml::profile` MUST NOT equal `"production"`.
//! 3. The caller MUST supply explicit confirmation (a `--yes` flag in
//!    the CLI; programmatic callers pass [`ResetRequest::confirmed`]
//!    `= true`).
//!    All three gates are enforced before any I/O. A refusal returns a
//!    typed [`ResetError::Refused`] so the operator-facing message is
//!    actionable.
//! # Logging-DB isolation
//! Per CLAUDE.md, the CRUD-log and event-log databases survive every
//! `db reset` invocation. Today the runner is single-context
//! so this module only operates on the application DB; the seam is
//! documented at [`reset_app_database`] for the day the three-database
//! `DjogiContext::pool_for(database)` API lands.
//! # `DROP DATABASE` connection plumbing
//! Postgres refuses to drop the database the current session is
//! connected to. We follow the standard libpq idiom: connect to the
//! `postgres` maintenance database with the same credentials, issue
//! `DROP DATABASE … WITH (FORCE)` (Postgres 13+; we target 18+ per
//! `docs/spec/decisions.md`), then `CREATE DATABASE …`. The forced
//! variant terminates other sessions to avoid the classic "another
//! session is connected" bounce.
//! After recreation the runner re-points at the fresh database via
//! [`crate::pg::pool::DjogiPool::connect`] and replays each migration
//! file pair in HISTORICAL apply order per the configuration spec.
//! The out-of-order policy allows a
//! hotfix migration to apply AFTER a later one, so lexical version
//! sort is NOT a faithful replay of what the live DB experienced.
//! `db reset` pre-flight reads `djogi_schema_migrations.applied_at`
//! BEFORE the drop, then uses that order during replay; versions
//! absent from the historical order (typically disk files added after
//! the last apply) sort lexically afterwards. Fresh DBs with no
//! ledger fall back to lexical sort safely.
//! # Historical-order capture error policy
//! The pre-flight capture step has TWO qualitatively different
//! failure modes that previously collapsed to the same outcome:
//! - **Ledger genuinely missing** (the `pg_class` probe returns
//!   `false`): legitimate fresh-DB fallback. Reset proceeds with an
//!   empty historical map, and `build_replay_plan` falls back to
//!   lexical version sort.
//! - **Anything else** — connection failure, query failure, decode
//!   failure, permission denied: opaque. Reset propagates as
//!   [`ResetError::HistoricalOrderCaptureFailed`] and refuses to
//!   drop / recreate.
//!   Every failure mode swallowed itself via `unwrap_or_default`
//!   at the call site, so a flaky ledger read on a populated DB still
//!   triggered the destructive operation. The fix is the
//!   [`HistoricalCaptureError`] split: `LedgerMissing` is the only
//!   legitimate fall-back signal; `Transient(DjogiError)` propagates.
//! # No regex
//! URL parsing reuses the byte-level extractor in
//! [`super::policy::extract_host`] for the localhost gate, plus a
//! minimal forward-scan helper to split out the `<host>/<dbname>` parts.
//! No regex engine, no regex notation.

// `ResetError` carries an embedded `RunnerError`, which itself embeds
// boxed and string-rich variants; the resulting `Result` payload
// exceeds clippy's default 128-byte threshold for `result_large_err`.
// Boxing the whole error type would force every caller to indirect
// through a heap allocation just to discriminate among the variants
// the signal-vs-cost tradeoff favours allowing the lint at file
// scope here, mirroring the same `#[allow(clippy::result_large_err)]`
// pattern that `crate::config` and `crate::migrate::projection` use.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tokio_postgres::NoTls;

use crate::config::MigrateConfig;
use crate::context::DjogiContext;
use crate::error::{DbError, DjogiError};
use crate::pg::pool::DjogiPool;

use super::compose::{
    DATE_ARRAY_HELPER_PRELUDE, NUMERIC_ARRAY_HELPER_PRELUDE, TSTZ_ARRAY_HELPER_PRELUDE,
    date_array_helper_operation, numeric_array_helper_operation, tstz_array_helper_operation,
};
use super::ledger::compute_checksum;
use super::naming::{down_filename, up_filename};
use super::policy::{OutOfOrderPolicy, is_localhost_connection};
use super::projection::BucketKey;
use super::replay_plan::{
    ReplayPlanLoadStatus,
    find_non_transactional_statement_shape as replay_find_non_transactional_statement_shape,
    load_committed_replay_plan,
};
use super::runner::{DriftBaseline, RunnerCtx, RunnerIdentity, apply_plan};
use super::segment::{MigrationPlan, Segment, SegmentKind};
use super::sql::OperationSql;
use super::target::{app_dirname, bucket_dir, migrations_root};

// ── Public types ──────────────────────────────────────────────────────────

/// Configuration handed to [`reset_app_database`].
/// Constructed by the CLI glue from the resolved workspace, the
/// loaded [`crate::config::DjogiConfig`], and the operator's `--yes`
/// flag.
pub struct ResetRequest<'a> {
    /// Workspace root — `migrations/<database>/<app>/` lives below it.
    pub workspace_root: &'a Path,
    /// The operator's full `DATABASE_URL` — used for the localhost
    /// gate and for deriving the maintenance + reconnection URLs.
    pub database_url: &'a str,
    /// `Djogi.toml::profile` — production refuses unconditionally.
    pub profile: &'a str,
    /// `true` when the operator passed `--yes` (or the programmatic
    /// caller has otherwise confirmed). The default is `false`; the
    /// runner refuses without it.
    pub confirmed: bool,
    /// `true` when the operator explicitly accepts replaying
    /// drifted on-disk SQL after the parity preflight reports that
    /// the current files no longer match the live ledger. Default:
    /// `false` — drift refuses before `DROP DATABASE`.
    pub allow_checksum_drift_reset: bool,
    /// Maintenance database name. Defaults to `"postgres"` when
    /// the caller has nothing more specific (the conventional
    /// administrative DB present on every cluster).
    pub maintenance_database: &'a str,
    /// Migration-engine config the runner consults during the replay
    /// phase. Operators rarely override this; the CLI default is the
    /// loaded `Djogi.toml::migrate` block.
    pub migrate_config: MigrateConfig,
    /// Optional pool pointing at the **audit DB** (`crud_log_url` in
    /// `Djogi.toml`, by default `crud_log` derived from
    /// `database.url`). When `Some`, every replayed migration writes
    /// one `djogi_ddl_audit` row per executed (non-metadata) segment
    /// so the audit trail captures the post-reset apply just as a
    /// regular `apply` would. When `None` the audit write is silently
    /// skipped — appropriate for adopters who have not yet provisioned
    /// the second DB OR for tests that only care about the app-side
    /// replay.
    /// **Why a raw `deadpool_postgres::Pool`:** mirrors
    /// [`super::runner::RunnerCtx::audit_pool`] so the replay
    /// orchestrator can pass the pool through without re-wrapping.
    /// See the doc on `RunnerCtx::audit_pool` for the rationale (the
    /// audit pool is internal substrate; `DjogiPool`'s wider invariants
    /// such as post-connect callbacks and status reporting are not
    /// needed for the audit-side context the runner builds).
    /// **Construction.** Production callers build this via
    /// [`super::resolve_audit_url`] + [`super::build_audit_pool`]. The
    /// CLI's `db reset` glue degrades to `None` (with a warn log) if
    /// audit URL resolution or pool construction fails — losing the
    /// audit row is preferable to refusing the destructive operation
    /// over a sibling-DB outage. Tests typically pass `None` unless
    /// they explicitly want to assert the per-segment audit-row
    /// behaviour.
    pub audit_pool: Option<deadpool_postgres::Pool>,
    /// Runner node identity for identity-bearing replay operations.
    /// When `Some(RunnerIdentity::SingleNodeDev)`, the reset is
    /// allowed to proceed; Phase 0 replay provisions node 1 after
    /// the identity-free bootstrap SQL succeeds, and later replayed
    /// migrations bind that node on the pinned runner session.
    /// When `Some(RunnerIdentity::Selected { id })`, reset refuses
    /// before destructive work because drop/recreate removes the old
    /// node registration.
    /// When `None` and not production profile, reset refuses with
    /// `ResetRefusal::MissingNodeIdentity` — the operator must pass
    /// `--single-node-dev`.
    pub runner_identity: Option<RunnerIdentity>,
}

/// Successful-reset report. Names every replayed migration so the
/// operator can confirm the post-reset state matches expectation.
#[derive(Debug, Clone)]
pub struct ResetReport {
    /// The application database name that was dropped + recreated.
    pub database: String,
    /// One entry per replayed migration version in apply order.
    pub replayed_versions: Vec<ReplayedMigration>,
}

/// Per-replay record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedMigration {
    /// Bucket — `(database, app)` identity.
    pub bucket: BucketKey,
    /// Version id — `V<ts>__<slug>`.
    pub version: String,
}

/// Which file side a checksum-parity issue concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetSqlSide {
    Up,
    Down,
}

impl ResetSqlSide {
    const fn as_str(self) -> &'static str {
        match self {
            ResetSqlSide::Up => "up",
            ResetSqlSide::Down => "down",
        }
    }
}

/// Why checksum parity could not be satisfied for one historical row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetChecksumParityProblem {
    Drift,
    MissingFile,
    UnsupportedBaseline,
}

/// One checksum-parity issue found before the destructive reset step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetChecksumParityIssue {
    /// Bucket whose current on-disk SQL no longer matches the live ledger.
    pub bucket: BucketKey,
    /// Migration version carrying the mismatch.
    pub version: String,
    /// Whether the issue concerns `up.sdjql` or `down.sdjql`.
    pub sql_side: ResetSqlSide,
    /// Checksum recorded on the live ledger before reset.
    pub ledger_checksum: String,
    /// Checksum computed from the current on-disk file, or `None`
    /// when the expected file is missing.
    pub on_disk_checksum: Option<String>,
    /// Why the parity preflight refused this historical row.
    pub problem: ResetChecksumParityProblem,
}

/// Why reset cannot prove faithful replay semantics from the committed
/// migration artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReplaySemanticsProblem {
    MissingReplayPlan,
    InvalidReplayPlan,
}

/// One replay-semantics issue found before the destructive reset step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReplaySemanticsIssue {
    /// Bucket carrying the affected migration.
    pub bucket: BucketKey,
    /// Migration version that cannot be replayed safely from disk.
    pub version: String,
    /// Statement class that requires non-transactional replay semantics.
    pub statement_shape: String,
    /// Why reset could not recover a committed replay plan.
    pub problem: ResetReplaySemanticsProblem,
}

/// Errors surfaced by [`reset_app_database`].
#[derive(Debug)]
pub enum ResetError {
    /// One of the three triple-gates rejected the request. The
    /// embedded variant names which gate refused so the operator
    /// message is precise.
    Refused(ResetRefusal),
    /// Connecting to the maintenance database failed (admin URL
    /// unreachable, credentials wrong, ssl handshake failed, …).
    MaintenanceConnectFailed { source: DjogiError },
    /// `DROP DATABASE` or `CREATE DATABASE` returned an error from the
    /// server — typically permission denied (the connecting role lacks
    /// CREATEDB) or another session is still connected.
    MaintenanceSqlFailed { sql: String, source: DjogiError },
    /// Connecting to the freshly-created database failed.
    AppConnectFailed { source: DjogiError },
    /// Walking `migrations/<database>/` failed (I/O error reading the
    /// committed migration tree).
    MigrationScanFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Reading one of the on-disk SQL files failed.
    SqlReadFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Replay of one migration via [`apply_plan`] failed. Carries the
    /// version id and the underlying runner error so the operator can
    /// see which migration broke the replay.
    ReplayFailed {
        version: String,
        source: super::runner::RunnerError,
    },
    /// The supplied `DATABASE_URL` is missing the database-name
    /// component that `db reset` would otherwise drop. Surfaces a
    /// typed error rather than a panic deep inside `replace_db_in_url`.
    DatabaseUrlMalformed { database_url: String },
    /// The decoded database name is not a valid Postgres identifier.
    /// Defence-in-depth against URL-injection — we percent-decode the
    /// path component first, then check the resulting bytes match the
    /// strict grammar (ASCII letter or underscore, followed by ASCII
    /// alphanumerics / underscores, up to 63 bytes). Anything else
    /// refuses BEFORE we splice the value into `DROP DATABASE` /
    /// `CREATE DATABASE` DDL. Surfaced separately from
    /// `DatabaseUrlMalformed` so the operator can tell "no database
    /// component" from "the component decodes to something we won't
    /// quote into DDL".
    InvalidDatabaseName { name: String },
    /// Workspace lock acquisition failed before the replay could run.
    WorkspaceLockFailed { source: super::guard::GuardError },
    /// Capturing the live ledger's
    /// historical apply order failed for a reason that is NOT
    /// "ledger table is missing on a fresh DB". Pre-fix every
    /// failure mode of the capture step (connection error, decode
    /// error, generic SQL error, …) collapsed to an empty map via
    /// `unwrap_or_default()`, which silently fell through to the
    /// drop / recreate path on a transient error. That re-opens the
    /// A flaky ledger read that swallows itself, then
    /// the destructive operation runs anyway against a database
    /// whose true state we never confirmed.
    /// Post-fix: the ONLY legitimate fall-back-to-lexical signal is
    /// the `pg_class` probe returning `false` (genuinely fresh DB
    /// or freshly-recreated DB without bootstrap yet). Every other
    /// failure mode propagates through this variant, refusing the
    /// destructive operation. The operator-facing message names the
    /// underlying `DjogiError` so the failure point is unambiguous.
    HistoricalOrderCaptureFailed { source: DjogiError },
}

/// Specific refusal kind for [`ResetError::Refused`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetRefusal {
    /// `DATABASE_URL` does not resolve to localhost.
    NotLocalhost { database_url: String },
    /// `Djogi.toml::profile = "production"`.
    ProductionProfile { profile: String },
    /// The caller did not supply explicit confirmation.
    NotConfirmed,
    /// The live ledger's checksums do not match the current on-disk
    /// migration files. Reset refuses before `DROP DATABASE` unless
    /// the operator explicitly overrides the drift gate.
    ChecksumParity {
        issues: Vec<ResetChecksumParityIssue>,
    },
    /// Reset cannot prove faithful replay semantics for at least one
    /// committed migration before the destructive drop/recreate.
    ReplaySemantics {
        issues: Vec<ResetReplaySemanticsIssue>,
    },
    /// Reset requires explicit node identity for identity-bearing
    /// replay operations, but none was provided. The operator must
    /// pass `--single-node-dev` (the only permitted fallback for
    /// destructive local reset) or explicitly supply `--node-id`.
    MissingNodeIdentity,
    /// Reset with a selected node identity is refused because
    /// destructive drop/recreate on an identity-bearing node could
    /// permanently lose registered state. Only single-node-dev mode
    /// is permitted for destructive reset; the operator should not
    /// pass `--node-id` when resetting locally.
    SelectedNodeRefused { node_id: i32 },
    /// Identity-free mode is refused for destructive reset.
    /// `IdentityFree` carries no session binding or node identity
    /// tracking, so a drop/recreate replay cannot be attributed to
    /// a specific node. Only `--single-node-dev` is permitted for
    /// destructive local reset.
    IdentityFreeRefused,
}

impl std::fmt::Display for ResetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResetError::Refused(r) => write!(f, "db reset refused: {r}"),
            ResetError::MaintenanceConnectFailed { source } => write!(
                f,
                "db reset: connect to maintenance database failed: {source}"
            ),
            ResetError::MaintenanceSqlFailed { sql, source } => {
                write!(f, "db reset: maintenance SQL `{sql}` failed: {source}")
            }
            ResetError::AppConnectFailed { source } => {
                write!(
                    f,
                    "db reset: connect to fresh app database failed: {source}"
                )
            }
            ResetError::MigrationScanFailed { path, source } => write!(
                f,
                "db reset: scanning {} for migration files failed: {source}",
                path.display(),
            ),
            ResetError::SqlReadFailed { path, source } => write!(
                f,
                "db reset: reading migration SQL at {} failed: {source}",
                path.display(),
            ),
            ResetError::ReplayFailed { version, source } => write!(
                f,
                "db reset: replay of `{version}` failed: {source}; the database \
                 has been recreated but is now in a partial state — fix the \
                 underlying issue and re-run db reset",
            ),
            ResetError::DatabaseUrlMalformed { database_url } => write!(
                f,
                "db reset: DATABASE_URL `{database_url}` does not contain a \
                 database-name component (no `/` after the host); db reset \
                 cannot derive the database name to drop"
            ),
            ResetError::InvalidDatabaseName { name } => write!(
                f,
                "db reset: decoded database name `{name}` is not a valid \
                 Postgres identifier (expected: ASCII letter or underscore, \
                 followed by ASCII alphanumerics or underscores, up to 63 \
                 bytes); db reset refuses to splice arbitrary bytes into \
                 DROP DATABASE / CREATE DATABASE DDL"
            ),
            ResetError::WorkspaceLockFailed { source } => {
                write!(f, "db reset: workspace lock acquisition failed: {source}")
            }
            ResetError::HistoricalOrderCaptureFailed { source } => write!(
                f,
                "db reset: capturing the live ledger's historical apply order \
                 failed: {source}; refusing to proceed with the destructive \
                 drop / recreate because we cannot confirm the live state — \
                 fix the underlying connection / query failure and re-run, or \
                 (if the database genuinely does not exist yet) create it \
                 first then re-run db reset"
            ),
        }
    }
}

impl std::fmt::Display for ResetRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResetRefusal::NotLocalhost { database_url } => write!(
                f,
                "DATABASE_URL is not localhost (got `{database_url}`); db reset is \
                 a destructive operation and must not be invoked against a remote \
                 database"
            ),
            ResetRefusal::ProductionProfile { profile } => write!(
                f,
                "Djogi.toml::profile = `{profile}`; db reset refuses to run on a \
                 production profile"
            ),
            ResetRefusal::NotConfirmed => f.write_str(
                "db reset requires explicit confirmation — pass `--yes` (or set \
                 ResetRequest::confirmed = true) to acknowledge that the entire \
                 application database will be dropped",
            ),
            ResetRefusal::ChecksumParity { issues } => {
                let rendered = issues
                    .iter()
                    .map(render_checksum_parity_issue)
                    .collect::<Vec<_>>()
                    .join("; ");
                write!(
                    f,
                    "db reset checksum parity preflight found drift against the live ledger: \
                     {rendered}; refusing destructive drop / recreate unless you pass \
                     `--allow-checksum-drift-reset` (or set \
                     `ResetRequest::allow_checksum_drift_reset = true`)"
                )
            }
            ResetRefusal::ReplaySemantics { issues } => {
                let rendered = issues
                    .iter()
                    .map(render_replay_semantics_issue)
                    .collect::<Vec<_>>()
                    .join("; ");
                write!(
                    f,
                    "db reset cannot prove faithful replay semantics for at least one committed \
                     migration: {rendered}; refusing destructive drop / recreate until the migration \
                     has a committed replay manifest or is replay-safe as a single transactional plan"
                )
            }
            ResetRefusal::MissingNodeIdentity => f.write_str(
                "db reset requires explicit node identity for identity-bearing \
                 replay operations — pass `--single-node-dev` (the only permitted \
                 fallback for destructive local reset) or supply `--node-id`",
            ),
            ResetRefusal::SelectedNodeRefused { node_id } => write!(
                f,
                "db reset with selected node {node_id} is refused — destructive \
                 drop/recreate on an identity-bearing node could permanently lose \
                 registered state; use `--single-node-dev` instead of `--node-id`"
            ),
            ResetRefusal::IdentityFreeRefused => f.write_str(
                "db reset with identity-free mode is refused — IdentityFree carries \
                 no session binding or node identity, so destructive drop/recreate \
                 replay cannot be attributed to a specific node; use `--single-node-dev`",
            ),
        }
    }
}

impl std::error::Error for ResetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResetError::MaintenanceConnectFailed { source } => Some(source),
            ResetError::MaintenanceSqlFailed { source, .. } => Some(source),
            ResetError::AppConnectFailed { source } => Some(source),
            ResetError::MigrationScanFailed { source, .. } => Some(source),
            ResetError::SqlReadFailed { source, .. } => Some(source),
            ResetError::ReplayFailed { source, .. } => Some(source),
            ResetError::WorkspaceLockFailed { source } => Some(source),
            ResetError::HistoricalOrderCaptureFailed { source } => Some(source),
            _ => None,
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────

/// Drop, recreate, and replay every committed migration against the
/// application database in `req.database_url`.
/// Triple-gated per the module docs. Returns a [`ResetReport`] on
/// success or a [`ResetError`] on any failure mode — including a
/// gate refusal, which is surfaced as `ResetError::Refused` rather
/// than as a successful no-op.
pub async fn reset_app_database(req: ResetRequest<'_>) -> Result<ResetReport, ResetError> {
    // 1. Triple gate — every gate runs BEFORE any I/O so a refusal
    // leaves zero side effects on the workspace OR the database.
    if !is_localhost_connection(req.database_url) {
        return Err(ResetError::Refused(ResetRefusal::NotLocalhost {
            database_url: req.database_url.to_string(),
        }));
    }
    if req.profile == "production" {
        return Err(ResetError::Refused(ResetRefusal::ProductionProfile {
            profile: req.profile.to_string(),
        }));
    }
    if !req.confirmed {
        return Err(ResetError::Refused(ResetRefusal::NotConfirmed));
    }

    // 1b. Node identity gate — destructive reset requires explicit
    // single-node-dev mode. The contract: "No production hardcoded/default
    // node 1; --single-node-dev is the ONLY default-node fallback and is
    // refused in production." Reset refuses selected-node identity because
    // drop/recreate on an identity-bearing node could permanently lose
    // registered state. Missing identity without --single-node-dev is also
    // refused — the operator must explicitly acknowledge the destructive
    // operation with identity awareness.
    match &req.runner_identity {
        None => {
            return Err(ResetError::Refused(ResetRefusal::MissingNodeIdentity));
        }
        Some(RunnerIdentity::Selected { id }) => {
            return Err(ResetError::Refused(ResetRefusal::SelectedNodeRefused {
                node_id: *id,
            }));
        }
        Some(RunnerIdentity::SingleNodeDev) => {
            // Single-node-dev is the only permitted mode for destructive reset.
            // This path proceeds to the database derivation below.
        }
        Some(RunnerIdentity::IdentityFree) => {
            return Err(ResetError::Refused(ResetRefusal::IdentityFreeRefused));
        }
    }

    // 2. Derive the app database name and the maintenance URL.
    // Two-step parse: first extract+percent-decode the path
    // component, then validate the decoded bytes against the strict
    // Postgres-identifier grammar. We refuse weird-looking names
    // BEFORE splicing them into `DROP DATABASE` / `CREATE DATABASE`
    // DDL — defence-in-depth against URL-injection where a crafted
    // URL like `postgres://localhost/'; DROP TABLE foo; --` could
    // otherwise reach the DDL builder. The maintenance database
    // name flows from operator config (`--maintenance-database`,
    // default `postgres`) so it's validated separately.
    let database =
        extract_database_from_url(req.database_url).ok_or(ResetError::DatabaseUrlMalformed {
            database_url: req.database_url.to_string(),
        })?;
    if !is_valid_pg_identifier(&database) {
        return Err(ResetError::InvalidDatabaseName { name: database });
    }
    if !is_valid_pg_identifier(req.maintenance_database) {
        return Err(ResetError::InvalidDatabaseName {
            name: req.maintenance_database.to_string(),
        });
    }
    let maintenance_url = replace_db_in_url(req.database_url, req.maintenance_database).ok_or(
        ResetError::DatabaseUrlMalformed {
            database_url: req.database_url.to_string(),
        },
    )?;

    // 3. Acquire workspace lock — replay mutates ledger state on the
    // fresh DB; concurrent compose / apply / repair operations
    // against the same workspace must not interleave with reset.
    let lock_path = req.workspace_root.join(super::guard::LOCK_FILE_NAME);
    let _guard = super::guard::acquire(&lock_path, super::guard::DEFAULT_TIMEOUT)
        .map_err(|e| ResetError::WorkspaceLockFailed { source: e })?;

    // 4. Capture the HISTORICAL apply order from
    // the live ledger BEFORE the drop. The out-of-order policy
    // allows a hotfix migration to apply AFTER a later one, e.g.
    // `applied_at` of `0001 < 0003 < 0002`. Lexical version-string
    // sort would replay them as `0001, 0002, 0003`, which is NOT
    // the sequence the live database actually experienced. If
    // `0002` only succeeded historically because `0003` was
    // already in place, lexical replay would re-apply it
    // out-of-order on a fresh DB — different state from what we
    // just dropped.
    // Strategy: pre-flight a read-only connection to the live DB,
    // query `djogi_schema_migrations` ordered by `applied_at`, and
    // capture `(bucket, version) -> rank`. We then use that rank
    // as the replay sort key. Versions absent from the historical
    // order (e.g. files added on disk after the last apply) sort
    // AFTER any historical entry, lexically among themselves.
    // Error-policy split:
    // `HistoricalCaptureError::LedgerMissing` is the ONLY legitimate
    // fall-back-to-lexical signal (`pg_class` probe returned false:
    // genuinely fresh DB). Every OTHER failure mode (connection
    // failure, decode failure, generic SQL error, permission
    // denied) surfaces as `Transient(..)` and propagates through
    // `ResetError::HistoricalOrderCaptureFailed`. every
    // error collapsed to `()` and the reset proceeded with an
    // empty map — which re-opened the for transient
    // failures (the empty map masquerades as "fresh DB with no
    // history" and the destructive drop / recreate runs anyway).
    let historical_entries = match capture_historical_replay_entries(req.database_url).await {
        Ok(entries) => entries,
        Err(HistoricalCaptureError::LedgerMissing) => Vec::new(),
        Err(HistoricalCaptureError::Transient(e)) => {
            return Err(ResetError::HistoricalOrderCaptureFailed { source: e });
        }
    };
    preflight_reset_checksum_parity(
        req.workspace_root,
        &database,
        &historical_entries,
        req.allow_checksum_drift_reset,
    )?;
    preflight_reset_replay_semantics(req.workspace_root, &database)?;
    let historical_order = build_historical_order(&historical_entries);

    // 5. Drop + recreate the application database via the maintenance
    // connection. A fresh tokio_postgres client is opened just for
    // the two DDLs — the maintenance pool is intentionally NOT
    // cached because db reset is interactive / one-shot.
    drop_and_create_database(&maintenance_url, &database).await?;

    // 6. Connect to the freshly-created application DB and replay
    // every committed migration.
    let pool = DjogiPool::connect(req.database_url)
        .await
        .map_err(|e| ResetError::AppConnectFailed { source: e })?;
    let mut ctx = DjogiContext::from_pool(pool);

    let buckets = scan_committed_migrations(req.workspace_root, &database)?;
    // Replay order = historical apply order
    // (`applied_at` ascending) for versions that have a historical
    // entry; lexical-after-historical for versions that do not.
    let replay_plan = build_replay_plan(&buckets, &historical_order);
    let mut replayed: Vec<ReplayedMigration> = Vec::new();

    for (bucket, version) in replay_plan {
        replay_one_migration(
            &mut ctx,
            req.workspace_root,
            &bucket,
            &version,
            &req.migrate_config,
            &_guard,
            req.audit_pool.as_ref(),
            req.runner_identity,
        )
        .await?;
        replayed.push(ReplayedMigration {
            bucket: bucket.clone(),
            version,
        });
    }

    Ok(ResetReport {
        database,
        replayed_versions: replayed,
    })
}

/// Internal error classifier for [`capture_historical_apply_order`]
/// The capture step has two qualitatively different failure modes:
/// - **`LedgerMissing`** — the `pg_class` probe came back `false`.
///   The connection succeeded, the catalog query succeeded, and the
///   answer was "no `djogi_schema_migrations` table here". This is
///   the legitimate fresh-DB / freshly-recreated-DB signal. The
///   caller falls back to lexical sort and the destructive drop /
///   recreate proceeds.
/// - **`Transient(DjogiError)`** — anything else: tokio_postgres
///   connect failure (DB unreachable, auth fail, network drop, DB
///   does not exist), `current_database()` query failure, probe
///   query failure, decode error, generic SELECT failure. None of
///   these prove the DB is fresh; they prove we cannot CONFIRM the
///   live state. The caller propagates as
///   `ResetError::HistoricalOrderCaptureFailed` and refuses to
///   drop / recreate.
///   The helper returned `Result<_, >` and `unwrap_or_default`
///   at the call site collapsed every failure mode to "empty map →
///   proceed with lexical fallback". That re-opened the
///   under a transient connection / query failure: the destructive
///   path runs against a database whose history we never read.
#[derive(Debug)]
enum HistoricalCaptureError {
    /// `pg_class` probe returned `false` — ledger genuinely absent.
    LedgerMissing,
    /// Connection / query / decode failure — treat as opaque, do
    /// NOT proceed with the destructive operation.
    Transient(DjogiError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoricalReplayEntry {
    bucket: BucketKey,
    version: String,
    status: String,
    checksum_up: String,
    checksum_down: Option<String>,
}

/// Capture the historical apply
/// order from the live ledger before the drop.
/// Connects to the application DB at `database_url`, probes for the
/// presence of `djogi_schema_migrations`, and (when present) queries
/// it ordered by `applied_at ASC, id ASC`. Returns a
/// `(bucket, version) -> rank` map where lower ranks applied first
/// historically.
/// The error classification is intentional and load-bearing:
/// - Probe says ledger absent → `Err(HistoricalCaptureError::LedgerMissing)`
///   (caller falls back to lexical).
/// - Anything else → `Err(HistoricalCaptureError::Transient(..))`
///   (caller propagates and refuses the destructive drop).
///   Only `Applied` / `Faked` / `Baseline` rows participate — `Pending`,
///   `Failed`, `RolledBack` do not represent migrations whose effect
///   the live DB carries forward.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
async fn capture_historical_apply_order(
    database_url: &str,
) -> Result<BTreeMap<(BucketKey, String), u64>, HistoricalCaptureError> {
    let entries = capture_historical_replay_entries(database_url).await?;
    Ok(build_historical_order(&entries))
}

fn build_historical_order(entries: &[HistoricalReplayEntry]) -> BTreeMap<(BucketKey, String), u64> {
    entries
        .iter()
        .enumerate()
        .map(|(rank, entry)| ((entry.bucket.clone(), entry.version.clone()), rank as u64))
        .collect()
}

#[allow(clippy::disallowed_methods)]
async fn capture_historical_replay_entries(
    database_url: &str,
) -> Result<Vec<HistoricalReplayEntry>, HistoricalCaptureError> {
    let (client, conn) = tokio_postgres::connect(database_url, NoTls)
        .await
        .map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "tokio-postgres connect failed during historical-order capture: {e}"
            ))))
        })?;
    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("[db reset] historical-order driver: {e}");
        }
    });

    // Resolve the active database name so the captured map's bucket
    // identity matches what `scan_committed_migrations` produces.
    let db_row = client
        .query_one("SELECT current_database()::text", &[])
        .await
        .map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "current_database() failed during historical-order capture: {e}"
            ))))
        })?;
    let database: String = db_row.try_get(0).map_err(|e| {
        HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
            "decoding current_database() result failed: {e}"
        ))))
    })?;

    // Probe the ledger table. THIS is the canonical fresh-DB decision
    // point: the connection succeeded AND the catalog query returned
    // a typed answer. A `false` here means the ledger has not been
    // bootstrapped yet — legitimate fresh-DB fallback. A failure of
    // the probe itself is opaque and propagates as Transient.
    let probe = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "pg_class probe for djogi_schema_migrations failed: {e}"
            ))))
        })?;
    let exists: bool = probe.try_get(0).map_err(|e| {
        HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
            "decoding pg_class probe result failed: {e}"
        ))))
    })?;
    if !exists {
        drop(client);
        let _ = driver.await;
        return Err(HistoricalCaptureError::LedgerMissing);
    }

    let rows = client
        .query(
            "SELECT version, app_label, status, checksum_up, checksum_down \
             FROM djogi_schema_migrations \
             WHERE status IN ('applied', 'faked', 'baseline') \
             ORDER BY applied_at ASC, id ASC",
            &[],
        )
        .await
        .map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "SELECT djogi_schema_migrations failed during historical-order capture: {e}"
            ))))
        })?;

    let mut out: Vec<HistoricalReplayEntry> = Vec::with_capacity(rows.len());
    for (rank, row) in rows.iter().enumerate() {
        let version: String = row.try_get("version").map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "decoding ledger version column failed at rank {rank}: {e}"
            ))))
        })?;
        let app_label: String = row.try_get("app_label").map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "decoding ledger app_label column failed at rank {rank}: {e}"
            ))))
        })?;
        let status: String = row.try_get("status").map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "decoding ledger status column failed at rank {rank}: {e}"
            ))))
        })?;
        let checksum_up: String = row.try_get("checksum_up").map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "decoding ledger checksum_up column failed at rank {rank}: {e}"
            ))))
        })?;
        let checksum_down: Option<String> = row.try_get("checksum_down").map_err(|e| {
            HistoricalCaptureError::Transient(DjogiError::Db(DbError::other(format!(
                "decoding ledger checksum_down column failed at rank {rank}: {e}"
            ))))
        })?;
        out.push(HistoricalReplayEntry {
            bucket: BucketKey {
                database: database.clone(),
                app: app_label,
            },
            version,
            status,
            checksum_up,
            checksum_down,
        });
    }
    drop(client);
    let _ = driver.await;
    Ok(out)
}

fn preflight_reset_checksum_parity(
    workspace_root: &Path,
    database: &str,
    historical_entries: &[HistoricalReplayEntry],
    allow_checksum_drift_reset: bool,
) -> Result<(), ResetError> {
    let issues = collect_checksum_parity_issues(workspace_root, database, historical_entries)?;
    if issues.is_empty() || allow_checksum_drift_reset {
        return Ok(());
    }
    Err(ResetError::Refused(ResetRefusal::ChecksumParity { issues }))
}

fn collect_checksum_parity_issues(
    workspace_root: &Path,
    database: &str,
    historical_entries: &[HistoricalReplayEntry],
) -> Result<Vec<ResetChecksumParityIssue>, ResetError> {
    let on_disk = super::target::scan_filesystem_with_files(workspace_root, Some(database))
        .map_err(|err| ResetError::MigrationScanFailed {
            path: migrations_root(workspace_root).join(database),
            source: err,
        })?;
    let mut issues = Vec::new();

    for entry in historical_entries {
        if entry.status == "baseline" {
            issues.push(ResetChecksumParityIssue {
                bucket: entry.bucket.clone(),
                version: entry.version.clone(),
                sql_side: ResetSqlSide::Up,
                ledger_checksum: entry.checksum_up.clone(),
                on_disk_checksum: None,
                problem: ResetChecksumParityProblem::UnsupportedBaseline,
            });
            continue;
        }

        let up_path = on_disk
            .get(&entry.bucket)
            .and_then(|versions| versions.get(&entry.version))
            .cloned()
            .unwrap_or_else(|| {
                bucket_dir(workspace_root, &entry.bucket).join(up_filename(&entry.version))
            });
        push_checksum_issue_if_needed(
            &mut issues,
            entry,
            ResetSqlSide::Up,
            &entry.checksum_up,
            &up_path,
        )?;

        if let Some(ledger_checksum_down) = entry.checksum_down.as_deref() {
            let down_path =
                bucket_dir(workspace_root, &entry.bucket).join(down_filename(&entry.version));
            push_checksum_issue_if_needed(
                &mut issues,
                entry,
                ResetSqlSide::Down,
                ledger_checksum_down,
                &down_path,
            )?;
        }
    }

    Ok(issues)
}

fn push_checksum_issue_if_needed(
    issues: &mut Vec<ResetChecksumParityIssue>,
    entry: &HistoricalReplayEntry,
    sql_side: ResetSqlSide,
    ledger_checksum: &str,
    path: &Path,
) -> Result<(), ResetError> {
    let on_disk_sql = match fs::read_to_string(path) {
        Ok(sql) => sql,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            issues.push(ResetChecksumParityIssue {
                bucket: entry.bucket.clone(),
                version: entry.version.clone(),
                sql_side,
                ledger_checksum: ledger_checksum.to_string(),
                on_disk_checksum: None,
                problem: ResetChecksumParityProblem::MissingFile,
            });
            return Ok(());
        }
        Err(e) => {
            return Err(ResetError::SqlReadFailed {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let on_disk_checksum = compute_committed_sql_checksum(&on_disk_sql, sql_side);
    if on_disk_checksum != ledger_checksum {
        issues.push(ResetChecksumParityIssue {
            bucket: entry.bucket.clone(),
            version: entry.version.clone(),
            sql_side,
            ledger_checksum: ledger_checksum.to_string(),
            on_disk_checksum: Some(on_disk_checksum),
            problem: ResetChecksumParityProblem::Drift,
        });
    }
    Ok(())
}

fn render_checksum_parity_issue(issue: &ResetChecksumParityIssue) -> String {
    let version_path = format!(
        "{}/{}/{}",
        issue.bucket.database,
        app_dirname(&issue.bucket.app),
        issue.version
    );
    match issue.problem {
        ResetChecksumParityProblem::Drift => format!(
            "{version_path} {} checksum drift: ledger `{}` vs on-disk `{}`",
            issue.sql_side.as_str(),
            issue.ledger_checksum,
            issue.on_disk_checksum.as_deref().unwrap_or("<missing>")
        ),
        ResetChecksumParityProblem::MissingFile => format!(
            "{version_path} {} file missing: ledger checksum `{}` has no on-disk peer",
            issue.sql_side.as_str(),
            issue.ledger_checksum
        ),
        ResetChecksumParityProblem::UnsupportedBaseline => format!(
            "{version_path} baseline checksum `{}` cannot be compared to migration file bytes; \
             db reset cannot establish safe parity for a baseline row",
            issue.ledger_checksum
        ),
    }
}

fn preflight_reset_replay_semantics(
    workspace_root: &Path,
    database: &str,
) -> Result<(), ResetError> {
    let buckets = scan_committed_migrations(workspace_root, database)?;
    let mut issues = Vec::new();

    for (bucket, versions) in &buckets {
        for version in versions {
            let replay_sql = read_replay_sql_files(workspace_root, bucket, version)?;
            let plan_status =
                replay_plan_status_for_reset(workspace_root, bucket, version, &replay_sql);
            if let Some(issue) =
                replay_semantics_issue_for_plan_status(bucket, version, &replay_sql, &plan_status)
            {
                issues.push(issue);
            }
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(ResetError::Refused(ResetRefusal::ReplaySemantics {
            issues,
        }))
    }
}

fn replay_plan_status_for_reset(
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
    replay_sql: &ReplaySqlFiles,
) -> ReplayPlanLoadStatus {
    load_committed_replay_plan(
        workspace_root,
        bucket,
        version,
        &replay_sql.checksum_up,
        replay_sql.checksum_down.as_deref(),
    )
}

fn load_reset_replay_plan(
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
    replay_sql: &ReplaySqlFiles,
) -> Result<Option<MigrationPlan>, ResetError> {
    let plan_status = replay_plan_status_for_reset(workspace_root, bucket, version, replay_sql);
    if let Some(issue) =
        replay_semantics_issue_for_plan_status(bucket, version, replay_sql, &plan_status)
    {
        return Err(ResetError::Refused(ResetRefusal::ReplaySemantics {
            issues: vec![issue],
        }));
    }
    match plan_status {
        ReplayPlanLoadStatus::Loaded(plan) => Ok(Some(plan)),
        ReplayPlanLoadStatus::Missing | ReplayPlanLoadStatus::Invalid(_) => Ok(None),
    }
}

fn replay_semantics_issue_for_plan_status(
    bucket: &BucketKey,
    version: &str,
    replay_sql: &ReplaySqlFiles,
    plan_status: &ReplayPlanLoadStatus,
) -> Option<ResetReplaySemanticsIssue> {
    let statement_shape = replay_find_non_transactional_statement_shape(&replay_sql.up_sql)?;
    let problem = match plan_status {
        ReplayPlanLoadStatus::Loaded(_) => return None,
        ReplayPlanLoadStatus::Missing => ResetReplaySemanticsProblem::MissingReplayPlan,
        ReplayPlanLoadStatus::Invalid(_) => ResetReplaySemanticsProblem::InvalidReplayPlan,
    };
    Some(ResetReplaySemanticsIssue {
        bucket: bucket.clone(),
        version: version.to_string(),
        statement_shape: statement_shape.to_string(),
        problem,
    })
}

fn render_replay_semantics_issue(issue: &ResetReplaySemanticsIssue) -> String {
    let version_path = format!(
        "{}/{}/{}",
        issue.bucket.database,
        app_dirname(&issue.bucket.app),
        issue.version
    );
    match issue.problem {
        ResetReplaySemanticsProblem::MissingReplayPlan => format!(
            "{version_path} contains `{}` but has no committed replay manifest",
            issue.statement_shape
        ),
        ResetReplaySemanticsProblem::InvalidReplayPlan => format!(
            "{version_path} contains `{}` but its committed replay manifest is missing or stale",
            issue.statement_shape
        ),
    }
}

/// Given the on-disk bucket map and the captured
/// historical apply order, produce the deterministic replay plan as a
/// flat `Vec<(BucketKey, String)>` in the order migrations should be
/// re-applied.
/// **Sort key** (lower wins): `(historical_rank.unwrap_or(u64::MAX),
/// bucket.database, bucket.app, version)`. Versions WITH a historical
/// rank apply first (in apply-order); versions WITHOUT (typically
/// disk files added after the last historical apply) apply last,
/// sorted lexically among themselves so re-running the reset
/// produces byte-identical output.
/// Pulled out as a free function so unit tests can pin every edge
/// case without standing up a live connection.
fn build_replay_plan(
    buckets: &BTreeMap<BucketKey, Vec<String>>,
    historical_order: &BTreeMap<(BucketKey, String), u64>,
) -> Vec<(BucketKey, String)> {
    let mut flat: Vec<(BucketKey, String)> = Vec::new();
    for (bucket, versions) in buckets {
        for v in versions {
            flat.push((bucket.clone(), v.clone()));
        }
    }
    flat.sort_by(|a, b| {
        let ra = historical_order
            .get(&(a.0.clone(), a.1.clone()))
            .copied()
            .unwrap_or(u64::MAX);
        let rb = historical_order
            .get(&(b.0.clone(), b.1.clone()))
            .copied()
            .unwrap_or(u64::MAX);
        ra.cmp(&rb)
            .then_with(|| a.0.database.cmp(&b.0.database))
            .then_with(|| a.0.app.cmp(&b.0.app))
            .then_with(|| a.1.cmp(&b.1))
    });
    flat
}

// ── Internals ─────────────────────────────────────────────────────────────

/// Tokio-postgres-based DROP + CREATE helper. Connects to the
/// maintenance database, issues both statements via `batch_execute`
/// (the simple-query protocol — Postgres refuses to prepare DROP /
/// CREATE DATABASE), and returns once both succeed.
#[allow(clippy::disallowed_methods)]
async fn drop_and_create_database(maintenance_url: &str, database: &str) -> Result<(), ResetError> {
    let (client, conn) = tokio_postgres::connect(maintenance_url, NoTls)
        .await
        .map_err(|e| ResetError::MaintenanceConnectFailed {
            source: DjogiError::Db(DbError::other(format!(
                "tokio-postgres connect to maintenance DB failed: {e}"
            ))),
        })?;
    // The connection task must run for the lifetime of the client.
    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::error!("[db reset] maintenance connection error: {e}");
        }
    });

    // Quote the database name for safety. Postgres identifier rules
    // allow `"` to appear inside a quoted identifier only as an
    // escaped `""` pair. We replace each `"` byte with `""` so an
    // operator who somehow has a quote in the database name still
    // gets a syntactically-valid identifier; in practice the database
    // grammar (set by the connection URL parser upstream) precludes
    // that, but the defensive escape is free.
    let quoted_db = quote_identifier(database);

    let drop_sql = format!("DROP DATABASE IF EXISTS {quoted_db} WITH (FORCE)");
    client
        .batch_execute(&drop_sql)
        .await
        .map_err(|e| ResetError::MaintenanceSqlFailed {
            sql: drop_sql.clone(),
            source: DjogiError::Db(DbError::other(format!("{e}"))),
        })?;

    let create_sql = format!("CREATE DATABASE {quoted_db}");
    client
        .batch_execute(&create_sql)
        .await
        .map_err(|e| ResetError::MaintenanceSqlFailed {
            sql: create_sql.clone(),
            source: DjogiError::Db(DbError::other(format!("{e}"))),
        })?;

    // Drop the client so the connection task finishes; await the task
    // so the connection close lands deterministically.
    drop(client);
    let _ = driver.await;
    Ok(())
}

/// Walk `migrations/<database>/` and collect every committed
/// `V<ts>__<slug>.sql` migration grouped by `(database, app)` bucket.
/// Returns a `BTreeMap` so iteration order is deterministic across
/// runs — key order is `(database, app)` ASCII-sorted; per-bucket
/// migration lists are version-sorted (lexical = chronological per
/// the [`super::naming`] convention).
/// Files matching the down-side suffix (`.down.sdjql`) are skipped
/// the up-side filename serves as the canonical version identifier.
fn scan_committed_migrations(
    workspace_root: &Path,
    database: &str,
) -> Result<BTreeMap<BucketKey, Vec<String>>, ResetError> {
    let with_paths = super::target::scan_filesystem_with_files(workspace_root, Some(database))
        .map_err(|err| ResetError::MigrationScanFailed {
            path: migrations_root(workspace_root).join(database),
            source: err,
        })?;
    Ok(with_paths
        .into_iter()
        .map(|(bucket, vers)| (bucket, vers.into_keys().collect()))
        .collect())
}

struct ReplaySqlFiles {
    up_sql: String,
    down_sql: String,
    checksum_up: String,
    checksum_down: Option<String>,
}

/// Classify SQL that must run outside a Postgres transaction, such
/// as `CREATE INDEX CONCURRENTLY` and `DROP INDEX CONCURRENTLY`.
///
/// This is the library surface for callers that need to preflight a
/// committed SQL file without duplicating the runner/reset parser.
pub fn find_non_transactional_statement_shape(sql: &str) -> Option<&'static str> {
    replay_find_non_transactional_statement_shape(sql)
}

/// Compute the canonical checksum of a committed migration SQL file's
/// contents, in the same domain compose uses when it records the ledger
/// `checksum_up` / `checksum_down` values.
/// # Why this exists
/// Compose computes checksums over the [`super::OperationSql`] fragments
/// (`label` + `up` / `down` SQL), NOT over the rendered file that those
/// fragments are written into. A composed migration file carries a
/// `-- Djogi composed migration — {up,down}` header, a
/// `-- DO NOT EDIT …` banner, and per-statement `-- <label>` comment
/// lines that are absent from the fragment domain. A naive
/// [`compute_checksum`] over the whole file therefore yields a different
/// digest than the ledger stores. This helper strips the file framing
/// (parsing the composed file back into its canonical fragments) so a
/// recomputed checksum matches what compose persisted — load-bearing for
/// `djogi migrations repair checksum-drift`, which recomputes from disk
/// when the operator omits `--checksum-up` / `--checksum-down`.
/// # Behavior
/// When `sql` is a recognizable composed file (correct header + banner),
/// the digest is computed over its canonical fragments for `side`.
/// Otherwise — a hand-authored or legacy file with no composed framing
/// it falls back to the whole-file digest, matching how such files are
/// checksummed elsewhere.
pub fn compute_committed_sql_checksum(sql: &str, side: ResetSqlSide) -> String {
    canonical_composed_sql_fragments(sql, side)
        .map(|fragments| compute_checksum(fragments.iter().map(String::as_str)))
        .unwrap_or_else(|| compute_checksum([sql]))
}

/// Compute the canonical checksum of a committed *down* SQL file, or
/// `None` when the down side carries no real statements.
/// Shares the fragment-level domain documented on
/// [`compute_committed_sql_checksum`]. Returns `None` (matching compose's
/// `NULL` `checksum_down` sentinel) when the down file is comment-only
/// either every composed fragment is a comment, or, for a non-composed
/// file, every non-blank line is a `--` comment. A down side with at
/// least one real statement returns `Some(digest)`.
/// Note this takes file *contents* already read from disk; a missing
/// down file (which also maps to a `None` / `NULL` down checksum) is the
/// caller's concern, not handled here.
pub fn compute_committed_down_sql_checksum(sql: &str) -> Option<String> {
    if let Some(fragments) = canonical_composed_sql_fragments(sql, ResetSqlSide::Down) {
        if fragments.iter().all(|fragment| fragment.starts_with("--")) {
            None
        } else {
            Some(compute_checksum(fragments.iter().map(String::as_str)))
        }
    } else if is_comment_only_sql(sql) {
        None
    } else {
        Some(compute_checksum([sql]))
    }
}

fn is_comment_only_sql(sql: &str) -> bool {
    sql.lines()
        .map(str::trim_start)
        .filter(|line| !line.trim().is_empty())
        .all(|line| line.starts_with("--"))
}

/// One statement fragment recovered from a committed composed SQL
/// file: the rendered `-- <label>` marker and the executable bytes
/// that follow it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommittedSqlFragment {
    /// Header label from the `-- <label>` marker.
    label: String,
    /// Fragment body that participates in compose canonical checksums.
    sql: String,
}

/// Canonical, labeled recovery of a composed migration file's fragments.
/// This function preserves helper prelude provenance by emitting synthetic
/// labels (`NumericArrayHelperPrelude`, etc.) for fragments that are
/// rendered without label lines in the SQL file.
fn canonical_composed_labeled_fragments(
    sql: &str,
    side: ResetSqlSide,
) -> Option<Vec<CommittedSqlFragment>> {
    let expected_header = match side {
        ResetSqlSide::Up => "-- Djogi composed migration — up\n",
        ResetSqlSide::Down => "-- Djogi composed migration — down\n",
    };
    if !sql.starts_with(expected_header) {
        return None;
    }

    let (_, mut body) =
        sql.split_once("-- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n")?;

    let mut fragments = Vec::new();
    let helper_pairs: [(&str, &'static str, String); 3] = match side {
        ResetSqlSide::Up => [
            (
                NUMERIC_ARRAY_HELPER_PRELUDE,
                "NumericArrayHelperPrelude",
                NUMERIC_ARRAY_HELPER_PRELUDE.to_string(),
            ),
            (
                DATE_ARRAY_HELPER_PRELUDE,
                "DateArrayHelperPrelude",
                DATE_ARRAY_HELPER_PRELUDE.to_string(),
            ),
            (
                TSTZ_ARRAY_HELPER_PRELUDE,
                "TstzArrayHelperPrelude",
                TSTZ_ARRAY_HELPER_PRELUDE.to_string(),
            ),
        ],
        ResetSqlSide::Down => [
            (
                NUMERIC_ARRAY_HELPER_PRELUDE,
                "NumericArrayHelperPrelude",
                numeric_array_helper_operation().down,
            ),
            (
                DATE_ARRAY_HELPER_PRELUDE,
                "DateArrayHelperPrelude",
                date_array_helper_operation().down,
            ),
            (
                TSTZ_ARRAY_HELPER_PRELUDE,
                "TstzArrayHelperPrelude",
                tstz_array_helper_operation().down,
            ),
        ],
    };
    for (rendered_prelude, label, checksum_fragment) in helper_pairs {
        if let Some(rest) = body.strip_prefix(rendered_prelude) {
            fragments.push(CommittedSqlFragment {
                label: label.to_string(),
                sql: checksum_fragment,
            });
            body = rest.strip_prefix('\n').unwrap_or(rest);
        }
    }

    let mut operation_fragments = parse_composed_operation_fragments(body, side)?;
    if side == ResetSqlSide::Down {
        operation_fragments.reverse();
    }
    fragments.extend(operation_fragments);
    Some(fragments)
}

fn canonical_composed_sql_fragments(sql: &str, side: ResetSqlSide) -> Option<Vec<String>> {
    canonical_composed_labeled_fragments(sql, side)
        .map(|fragments| fragments.into_iter().map(|fragment| fragment.sql).collect())
}

fn parse_composed_operation_fragments(
    body: &str,
    side: ResetSqlSide,
) -> Option<Vec<CommittedSqlFragment>> {
    let mut rest = body.trim_end_matches('\n');
    let mut fragments = Vec::new();
    if rest.trim().is_empty() {
        return Some(fragments);
    }

    loop {
        let after_dashes = rest.strip_prefix("-- ")?;
        let (label, after_label) = after_dashes.split_once('\n')?;
        let (fragment, next) = match after_label.find("\n\n-- ") {
            Some(next_label) => (
                &after_label[..next_label],
                Some(&after_label[next_label + 2..]),
            ),
            None => (after_label, None),
        };
        let sql = if side == ResetSqlSide::Down {
            fragment
                .lines()
                .filter(|line| !line.starts_with("-- LOSSY:"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            fragment.to_string()
        };
        fragments.push(CommittedSqlFragment {
            label: label.to_string(),
            sql,
        });
        match next {
            Some(next_rest) => rest = next_rest,
            None => break,
        }
    }
    Some(fragments)
}

/// Fallback replay plan rebuilt from committed SQL files when no committed
/// replay-plan sidecar (`<version>.plan.json`) is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackReplayPlan {
    /// Single-transactional plan whose up fragments rehash exactly to
    /// `checksum_up` under the runner's checksum verifier.
    pub plan: MigrationPlan,
    /// Canonical up checksum computed from
    /// [`compute_committed_sql_checksum`].
    pub checksum_up: String,
    /// Canonical down checksum computed from
    /// [`compute_committed_down_sql_checksum`].
    pub checksum_down: Option<String>,
}

/// Error conditions while constructing a fallback replay plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReplayPlanError {
    /// The SQL must run in an exclusive transaction; non-transactional
    /// shapes require a committed sidecar.
    NonTransactionalStatement { shape: &'static str },
}

/// Build a replay plan from committed SQL when the sidecar is missing.
///
/// The canonical checksum domains for compose output are based on
/// executable fragments, not rendered text. This builder recovers those
/// canonical fragments so the runner's checksum verification sees the
/// same statement set it recomputes from `plan.statements`.
///
/// * For composed up SQL, one `OperationSql` is emitted per recovered
///   canonical fragment (including helper-prelude fragments that are
///   rendered without `-- <label>` lines).
/// * For hand-authored/non-composed SQL, the legacy shape remains a
///   single whole-file statement and whole-file checksums.
///
/// `down` checksums intentionally follow compose's canonical semantics:
/// missing or comment-only down content yields `None` while executable
/// down SQL yields `Some`.
/// This is intentionally asymmetric with compose-sidecar replay: when fallbacks
/// recompute `checksum_down`, they use the current committed down file contents
/// and do **not** preserve any compose-sidecar value from the original
/// application.
///
/// # Why this API exists
/// Without this reconstruction, composing-only fallbacks can pair a
/// single rendered whole-file statement with canonical fragment checksums,
/// and the runner rejects the replay during checksum verification.
///
/// # Where this is used
/// Intended for CLI no-sidecar apply fallback and for reset replay when
/// a committed replay-plan sidecar is missing.
///
/// # Error conditions
/// Returns [`FallbackReplayPlanError::NonTransactionalStatement`] when
/// `up_sql` contains a non-transactional statement shape (for example
/// `CREATE INDEX CONCURRENTLY`), because the fallback API cannot safely
/// preserve the original non-transactional execution shape without the
/// committed sidecar.
pub fn canonical_fallback_replay_plan(
    bucket: &BucketKey,
    version: &str,
    up_sql: &str,
    down_sql: &str,
) -> Result<FallbackReplayPlan, FallbackReplayPlanError> {
    if let Some(shape) = replay_find_non_transactional_statement_shape(up_sql) {
        return Err(FallbackReplayPlanError::NonTransactionalStatement { shape });
    }

    let checksum_up = compute_committed_sql_checksum(up_sql, ResetSqlSide::Up);
    let checksum_down = compute_committed_down_sql_checksum(down_sql);

    let statements = match canonical_composed_labeled_fragments(up_sql, ResetSqlSide::Up) {
        Some(fragments) => fragments
            .into_iter()
            .map(|fragment| OperationSql {
                label: fragment.label,
                up: fragment.sql,
                down: String::new(),
                lossy: None,
            })
            .collect(),
        None => vec![OperationSql {
            label: format!("replay {version}"),
            up: up_sql.to_string(),
            down: down_sql.to_string(),
            lossy: None,
        }],
    };

    Ok(FallbackReplayPlan {
        plan: MigrationPlan {
            bucket: bucket.clone(),
            classification: super::diff::Classification::Additive,
            segments: vec![Segment {
                kind: SegmentKind::Transactional,
                statements,
            }],
        },
        checksum_up,
        checksum_down,
    })
}

fn read_replay_sql_files(
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
) -> Result<ReplaySqlFiles, ResetError> {
    let bucket_dir = super::target::bucket_dir(workspace_root, bucket);
    let up_path = bucket_dir.join(up_filename(version));
    let down_path = bucket_dir.join(down_filename(version));

    let up_sql = fs::read_to_string(&up_path).map_err(|e| ResetError::SqlReadFailed {
        path: up_path.clone(),
        source: e,
    })?;
    let checksum_up = compute_committed_sql_checksum(&up_sql, ResetSqlSide::Up);

    let (down_sql, checksum_down) = match fs::read_to_string(&down_path) {
        Ok(sql) => {
            let checksum_down = compute_committed_down_sql_checksum(&sql);
            (sql, checksum_down)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => {
            return Err(ResetError::SqlReadFailed {
                path: down_path,
                source: e,
            });
        }
    };

    Ok(ReplaySqlFiles {
        up_sql,
        down_sql,
        checksum_up,
        checksum_down,
    })
}

/// Read one migration's committed SQL and apply it through the runner.
/// Manifest-backed migrations replay the committed segment plan so
/// non-transactional statements keep their original execution shape.
/// Legacy migrations without a manifest fall back to a single
/// transactional segment. In both paths, the runner receives the same
/// canonical operation-fragment checksum domain that compose records
/// in the ledger.
#[expect(
    clippy::too_many_arguments,
    reason = "reset replay bridges committed-file context into the runner without hiding ownership"
)]
async fn replay_one_migration(
    ctx: &mut DjogiContext,
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
    migrate_config: &MigrateConfig,
    guard: &super::guard::WorkspaceGuard,
    audit_pool: Option<&deadpool_postgres::Pool>,
    runner_identity: Option<RunnerIdentity>,
) -> Result<(), ResetError> {
    let replay_sql = read_replay_sql_files(workspace_root, bucket, version)?;

    let plan = match load_reset_replay_plan(workspace_root, bucket, version, &replay_sql)? {
        Some(plan) => plan,
        None => {
            canonical_fallback_replay_plan(
                bucket,
                version,
                &replay_sql.up_sql,
                &replay_sql.down_sql,
            )
            .map_err(
                |FallbackReplayPlanError::NonTransactionalStatement { shape }| {
                    ResetError::Refused(ResetRefusal::ReplaySemantics {
                        issues: vec![ResetReplaySemanticsIssue {
                            bucket: bucket.clone(),
                            version: version.to_string(),
                            statement_shape: shape.to_string(),
                            problem: ResetReplaySemanticsProblem::MissingReplayPlan,
                        }],
                    })
                },
            )?
            .plan
        }
    };

    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: version.to_string(),
        description: format!("db reset replay of {version}"),
        checksum_up: replay_sql.checksum_up,
        checksum_down: replay_sql.checksum_down,
        snapshot: None,
        snapshot_path: None,
        // `MigrateConfig` does not derive `Clone` (the type carries
        // a small fixed-size payload but the wider stance is
        // to construct it explicitly per call so future changes
        // surface at every callsite). We mirror the operator's
        // settings into a fresh instance.
        config: MigrateConfig {
            concurrent_warn_relpages: migrate_config.concurrent_warn_relpages,
            strict_concurrent_warnings: migrate_config.strict_concurrent_warnings,
            pk_flip_long_tx_threshold_secs: migrate_config.pk_flip_long_tx_threshold_secs,
            pk_flip_join_table_option: migrate_config.pk_flip_join_table_option,
        },
        // Replay applies in lexical order, so the runner's
        // out-of-order detection should never trip — but supplying
        // `AllowWithDiagnostic` matches the usual dev default and
        // means a bug here surfaces as a warning rather than a hard
        // failure during the time-sensitive reset window.
        out_of_order_policy: OutOfOrderPolicy::AllowWithDiagnostic,
        // Production wire-up. When the
        // caller supplied an audit pool on `ResetRequest::audit_pool`
        // we plumb it through to `RunnerCtx` so each replayed
        // migration writes one `djogi_ddl_audit` row per executed
        // segment, exactly as a regular `apply` would. `cloned()`
        // bumps the underlying `Arc` (deadpool pools are Arc-shaped)
        // so the runner's per-segment context can take ownership of
        // its own handle without disturbing the orchestrator's. When
        // the caller passed `None` the runner's audit-write loop
        // gracefully skips — matching the runner's own best-effort
        // stance documented on `record_ddl_audit_for_plan`.
        audit_pool: audit_pool.cloned(),
        // Reset replay inherits the identity from ResetRequest.
        // The pre-drop identity gate ensures only SingleNodeDev reaches
        // this code path (None, Selected, and IdentityFree are refused earlier).
        // Phase 0 replay uses that identity to provision node 1 before
        // marking the bootstrap row applied; later replayed migrations
        // bind the provisioned node normally.
        runner_identity,
        drift_baseline: DriftBaseline::Disabled,
    };

    apply_plan(ctx, &plan, &runner_ctx, guard)
        .await
        .map_err(|e| ResetError::ReplayFailed {
            version: version.to_string(),
            source: e,
        })?;
    Ok(())
}

// ── URL helpers ───────────────────────────────────────────────────────────

/// Extract the database-name component from a Postgres URL.
/// Returns `None` when the URL has no path-component database name
/// (e.g. `postgres://localhost`) — `db reset` cannot derive a database
/// to drop in that case.
/// **Percent-decoding.** The path-component bytes are percent-decoded
/// before being returned: a path of `my%2Fdb` decodes to `my/db`.
/// Without this step a URL like `postgres://localhost/my%2Fdb` would
/// make the runner drop the literal identifier `my%2Fdb` (with a
/// `%2F` byte sequence in it) while the post-recreate reconnection
/// would target the correctly-decoded `my/db` — different databases.
/// Decoding produces the SAME byte sequence libpq itself sees when it
/// connects, so the maintenance-DB DROP target matches what the runner
/// re-connects to. Returns `None` on malformed escapes (a `%` not
/// followed by two hex digits) — refusing rather than guessing keeps
/// the destructive path defensive.
/// Validation against the Postgres identifier grammar (ASCII letter
/// or underscore followed by ASCII alphanumerics or underscores, up
/// to 63 bytes) is layered on top by [`is_valid_pg_identifier`]
/// extraction returns the raw decoded string so error messages can
/// surface what the operator actually supplied.
/// **No regex.** Walks the URL bytes from the rightmost `/` once and
/// then walks the path bytes once more during the percent-decode.
fn extract_database_from_url(url: &str) -> Option<String> {
    // Confirm the URL has a recognised scheme. We accept both
    // `postgres://` and `postgresql://`. Anything else is treated as
    // libpq parameter form, which db reset doesn't support today
    // (the operator would need the URL form for the libpq path).
    let body = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))?;
    // Skip past the authority — the database name follows the FIRST
    // `/` after the scheme. Authority byte indexing within `body`
    // walks until that slash or end-of-string.
    let mut idx = 0usize;
    let body_bytes = body.as_bytes();
    while idx < body_bytes.len() && body_bytes[idx] != b'/' {
        idx += 1;
    }
    if idx >= body_bytes.len() {
        return None; // no path component
    }
    // Path starts after the slash; database name runs until the next
    // `?` (query parameters) or end-of-string.
    let path_start = idx + 1;
    let mut path_end = path_start;
    while path_end < body_bytes.len() && body_bytes[path_end] != b'?' {
        path_end += 1;
    }
    if path_end == path_start {
        return None; // empty database name
    }
    percent_decode_strict(&body_bytes[path_start..path_end])
}

/// Percent-decode a byte slice strictly. A `%` must be followed by
/// exactly two hex digits (case-insensitive ASCII); any other shape
/// returns `None`. Output is treated as UTF-8 — non-UTF-8 byte
/// sequences also return `None`.
/// Kept private to this module so the destructive `db reset` path is
/// the only consumer; if a future caller needs the same primitive we
/// can promote it without churning the public surface.
fn percent_decode_strict(bytes: &[u8]) -> Option<String> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            // Need exactly two hex digits after the `%`.
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_digit_value(bytes[i + 1])?;
            let lo = hex_digit_value(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Map an ASCII hex digit byte to its 0..=15 value. Returns `None`
/// for any non-hex byte — used by [`percent_decode_strict`] to refuse
/// malformed escapes outright.
fn hex_digit_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

/// Validate a string against the strict Postgres-identifier grammar:
/// > ASCII letter or underscore, followed by zero-or-more ASCII
/// > alphanumerics or underscores, up to 63 bytes total.
/// > **No regex.** Byte-level checks per `docs/spec/decisions.md`
/// > `u8::is_ascii_alphabetic`, `u8::is_ascii_alphanumeric`, and
/// > explicit byte equality against `b'_'`.
/// > Postgres' own grammar is technically more permissive (it accepts
/// > any byte sequence inside double-quoted identifiers), but the
/// > grammar above is the one every Djogi-emitted identifier obeys.
/// > Refusing anything wider keeps the `DROP DATABASE` /
/// > `CREATE DATABASE` paths free of operator-supplied bytes that the
/// > double-quote escape elsewhere in the codebase wouldn't otherwise
/// > surface.
fn is_valid_pg_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    // Length: 1..=63 bytes (the standard Postgres `NAMEDATALEN - 1`).
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    // Leading byte: ASCII letter or underscore.
    let first = bytes[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return false;
    }
    // Trailing bytes: ASCII alphanumerics or underscore.
    for &b in &bytes[1..] {
        if !b.is_ascii_alphanumeric() && b != b'_' {
            return false;
        }
    }
    true
}

/// Replace the database-name component in a Postgres URL with a new
/// value. Preserves the scheme, authority, and any trailing query
/// string.
/// Returns `None` when the URL has no recognisable database component.
/// Visible to the rest of the crate so the seed runner (
/// can reuse the same splice — `db seed --database <name>` derives
/// the per-database connection URL from the application URL by
/// replacing the path component in place.
pub fn replace_db_in_url(url: &str, new_db: &str) -> Option<String> {
    let body = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))?;
    let scheme = if url.starts_with("postgres://") {
        "postgres://"
    } else {
        "postgresql://"
    };
    // Find the path slash.
    let mut idx = 0usize;
    let body_bytes = body.as_bytes();
    while idx < body_bytes.len() && body_bytes[idx] != b'/' {
        idx += 1;
    }
    if idx >= body_bytes.len() {
        return None;
    }
    let authority = &body[..idx];
    // Capture any trailing `?query` from the original path.
    let path_start = idx + 1;
    let mut path_end = path_start;
    while path_end < body_bytes.len() && body_bytes[path_end] != b'?' {
        path_end += 1;
    }
    let trailing = &body[path_end..]; // includes leading `?` if present, else empty.
    Some(format!("{scheme}{authority}/{new_db}{trailing}"))
}

/// Quote a Postgres identifier for embedding in DDL. Doubles each
/// internal `"` byte and wraps the result in `"`. The byte-level
/// approach matches the rest of the migrate substrate.
fn quote_identifier(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for b in name.bytes() {
        if b == b'"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(b as char);
        }
    }
    out.push('"');
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("djogi-reset-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn req<'a>(
        workspace: &'a Path,
        url: &'a str,
        profile: &'a str,
        confirmed: bool,
    ) -> ResetRequest<'a> {
        ResetRequest {
            workspace_root: workspace,
            database_url: url,
            profile,
            confirmed,
            allow_checksum_drift_reset: false,
            maintenance_database: "postgres",
            migrate_config: MigrateConfig::default(),
            // Gate / URL / replay tests do not assert audit-row
            // behaviour — the focused audit-pool wire-up coverage
            // lives in `tests/internal/sources/phase8_5_c2_118_*`.
            audit_pool: None,
            // Default to SingleNodeDev so existing tests pass the
            // identity gate. The dedicated identity gate tests
            // override this to None or Selected { id } to verify
            // the refusal paths.
            runner_identity: Some(RunnerIdentity::SingleNodeDev),
        }
    }

    /// Gate 1 — non-localhost URLs must refuse before any I/O.
    #[tokio::test]
    async fn refuses_when_url_is_not_localhost() {
        let work = temp_root("not_localhost");
        let res = reset_app_database(req(
            &work,
            "postgres://prod.example.com:5432/main",
            "development",
            true,
        ))
        .await;
        match res {
            Err(ResetError::Refused(ResetRefusal::NotLocalhost { database_url })) => {
                assert_eq!(database_url, "postgres://prod.example.com:5432/main");
            }
            other => panic!("expected NotLocalhost refusal, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Gate 2 — production profile refuses even against localhost
    /// (production-looking infra running locally is still production
    /// from a policy perspective).
    #[tokio::test]
    async fn refuses_when_profile_is_production() {
        let work = temp_root("production");
        let res =
            reset_app_database(req(&work, "postgres://localhost/main", "production", true)).await;
        match res {
            Err(ResetError::Refused(ResetRefusal::ProductionProfile { profile })) => {
                assert_eq!(profile, "production");
            }
            other => panic!("expected ProductionProfile refusal, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Gate 3 — unconfirmed invocation refuses. The default for
    /// `confirmed` (in CLI usage: when `--yes` is absent) is `false`.
    #[tokio::test]
    async fn refuses_when_not_confirmed() {
        let work = temp_root("not_confirmed");
        let res = reset_app_database(req(
            &work,
            "postgres://localhost/main",
            "development",
            false,
        ))
        .await;
        match res {
            Err(ResetError::Refused(ResetRefusal::NotConfirmed)) => {}
            other => panic!("expected NotConfirmed refusal, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Gate ordering — a request that fails multiple gates should
    /// surface the FIRST gate's refusal (localhost > production >
    /// confirmation). Operators get a single, deterministic refusal
    /// reason rather than a moving target.
    #[tokio::test]
    async fn gates_evaluate_in_documented_order() {
        let work = temp_root("gate_order");
        // Non-localhost + production + unconfirmed — every gate
        // refuses. The localhost gate should fire first.
        let res = reset_app_database(req(
            &work,
            "postgres://prod.example.com/main",
            "production",
            false,
        ))
        .await;
        match res {
            Err(ResetError::Refused(ResetRefusal::NotLocalhost { .. })) => {}
            other => panic!("expected NotLocalhost first, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    // ── Node identity gate tests ────────────────────────────────

    /// Identity gate 1 — missing identity refuses before destructive drop.
    #[tokio::test]
    async fn refuses_when_node_identity_missing() {
        let work = temp_root("missing_identity");
        let mut r = req(&work, "postgres://localhost/main", "development", true);
        r.runner_identity = None;
        let res = reset_app_database(r).await;
        match res {
            Err(ResetError::Refused(ResetRefusal::MissingNodeIdentity)) => {}
            other => panic!("expected MissingNodeIdentity refusal, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Identity gate 2 — selected node identity refuses destructive reset.
    #[tokio::test]
    async fn refuses_when_selected_node_set() {
        use crate::migrate::runner::RunnerIdentity;
        let work = temp_root("selected_node_refused");
        let mut r = req(&work, "postgres://localhost/main", "development", true);
        r.runner_identity = Some(RunnerIdentity::Selected { id: 7 });
        let res = reset_app_database(r).await;
        match res {
            Err(ResetError::Refused(ResetRefusal::SelectedNodeRefused { node_id })) => {
                assert_eq!(node_id, 7);
            }
            other => panic!("expected SelectedNodeRefused refusal, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Identity gate 3 — single-node-dev identity passes the identity gate.
    /// (The test hits the database derivation phase after identity check.)
    #[tokio::test]
    async fn single_node_dev_passes_identity_gate() {
        use crate::migrate::runner::RunnerIdentity;
        let work = temp_root("single_node_dev");
        // Use non-localhost URL so we hit NotLocalhost refusal instead of
        // MissingNodeIdentity — this proves the identity gate passed.
        let mut r = req(
            &work,
            "postgres://prod.example.com/main",
            "development",
            true,
        );
        r.runner_identity = Some(RunnerIdentity::SingleNodeDev);
        let res = reset_app_database(r).await;
        match res {
            Err(ResetError::Refused(ResetRefusal::NotLocalhost { .. })) => {
                // Identity gate passed — refused at localhost gate instead
            }
            other => panic!("expected NotLocalhost after identity gate passes, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Identity gate 4 — identity-free mode is refused by the identity gate.
    #[tokio::test]
    async fn identity_free_refused_by_identity_gate() {
        use crate::migrate::runner::RunnerIdentity;
        let work = temp_root("identity_free");
        // Use localhost URL so we pass the localhost gate and reach the identity gate.
        let mut r = req(&work, "postgres://localhost/main", "development", true);
        r.runner_identity = Some(RunnerIdentity::IdentityFree);
        let res = reset_app_database(r).await;
        match res {
            Err(ResetError::Refused(ResetRefusal::IdentityFreeRefused)) => {
                // Expected — IdentityFree refused by identity gate
            }
            other => panic!("expected IdentityFreeRefused, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn extract_database_from_url_basic() {
        assert_eq!(
            extract_database_from_url("postgres://localhost/main"),
            Some("main".to_string())
        );
        assert_eq!(
            extract_database_from_url("postgres://user:pass@localhost:5432/main"),
            Some("main".to_string())
        );
        assert_eq!(
            extract_database_from_url("postgresql://localhost/main?sslmode=disable"),
            Some("main".to_string())
        );
    }

    #[test]
    fn extract_database_from_url_missing_path() {
        // Authority-only URL (no path component) → None.
        assert_eq!(extract_database_from_url("postgres://localhost"), None);
        // Trailing slash but empty database → None.
        assert_eq!(extract_database_from_url("postgres://localhost/"), None);
        // Non-postgres scheme → None.
        assert_eq!(extract_database_from_url("mysql://localhost/main"), None);
    }

    /// Percent-decoding has to happen BEFORE the
    /// runner sees the database name, otherwise the maintenance-DB
    /// drop and the post-recreate reconnect target two different
    /// strings (one literal-percent, one decoded). The extractor MUST
    /// surface the decoded string so the validator can refuse it.
    #[test]
    fn extract_database_from_url_percent_decodes_path_bytes() {
        // `my%2Fdb` decodes to `my/db` — this is exactly what libpq
        // sees when it parses the same URL.
        assert_eq!(
            extract_database_from_url("postgres://localhost/my%2Fdb"),
            Some("my/db".to_string())
        );
        // Mixed case hex digits round-trip identically.
        assert_eq!(
            extract_database_from_url("postgres://localhost/foo%2fbar"),
            Some("foo/bar".to_string())
        );
        // A multi-byte UTF-8 sequence (`é` = `c3 a9`) decodes back to
        // the same sequence the operator would have typed unencoded.
        assert_eq!(
            extract_database_from_url("postgres://localhost/caf%C3%A9"),
            Some("café".to_string())
        );
    }

    /// Malformed `%XX` escapes must refuse rather
    /// than silently fall through to a literal `%`. A `%Z9` is not a
    /// valid escape; we don't pretend it is.
    #[test]
    fn extract_database_from_url_rejects_malformed_percent_escapes() {
        // Trailing `%` with no following bytes.
        assert_eq!(
            extract_database_from_url("postgres://localhost/main%"),
            None
        );
        // `%` followed by a single hex digit, then end-of-string.
        assert_eq!(
            extract_database_from_url("postgres://localhost/main%2"),
            None
        );
        // Non-hex bytes after the `%`.
        assert_eq!(
            extract_database_from_url("postgres://localhost/main%ZZ"),
            None
        );
    }

    /// The strict-identifier grammar covers the
    /// happy path (typical names) and refuses anything we won't
    /// emit into DDL.
    #[test]
    fn is_valid_pg_identifier_accepts_typical_names() {
        assert!(is_valid_pg_identifier("main"));
        assert!(is_valid_pg_identifier("crud_log"));
        assert!(is_valid_pg_identifier("event_log"));
        assert!(is_valid_pg_identifier("_underscore_lead"));
        assert!(is_valid_pg_identifier("a"));
        assert!(is_valid_pg_identifier("MyDatabase42"));
        // Boundary — exactly 63 bytes is accepted (the Postgres
        // NAMEDATALEN-1 limit).
        let sixty_three: String = std::iter::repeat_n('a', 63).collect();
        assert!(is_valid_pg_identifier(&sixty_three));
    }

    #[test]
    fn is_valid_pg_identifier_refuses_invalid_inputs() {
        // Empty.
        assert!(!is_valid_pg_identifier(""));
        // 64 bytes — one over the limit.
        let sixty_four: String = std::iter::repeat_n('a', 64).collect();
        assert!(!is_valid_pg_identifier(&sixty_four));
        // Leading digit.
        assert!(!is_valid_pg_identifier("1main"));
        // Internal slash (this is what `my%2Fdb` percent-decodes to).
        assert!(!is_valid_pg_identifier("my/db"));
        // SQL-injection shape — a single quote is not an identifier
        // byte, so the validator rejects the whole string.
        assert!(!is_valid_pg_identifier("'; DROP TABLE foo; --"));
        // Spaces.
        assert!(!is_valid_pg_identifier("my db"));
        // Hyphen.
        assert!(!is_valid_pg_identifier("my-db"));
        // Multi-byte UTF-8 (`café`).
        assert!(!is_valid_pg_identifier("café"));
    }

    /// `reset_app_database` must surface
    /// `InvalidDatabaseName` rather than splicing decoded bytes into
    /// DDL. We exercise three failure shapes plus the maintenance-DB
    /// override path through the public entry.
    #[tokio::test]
    async fn reset_refuses_when_decoded_database_name_is_not_an_identifier() {
        // `my%2Fdb` decodes to `my/db` — gate-passing localhost URL
        // but the decoded name fails the identifier grammar.
        let work = temp_root("invalid_decoded");
        let res = reset_app_database(req(
            &work,
            "postgres://localhost/my%2Fdb",
            "development",
            true,
        ))
        .await;
        match res {
            Err(ResetError::InvalidDatabaseName { name }) => assert_eq!(name, "my/db"),
            other => panic!("expected InvalidDatabaseName for `my/db`, got {other:?}"),
        }

        // The `--maintenance-database` operator-supplied value flows
        // through the same validator; a crafted value must refuse.
        let bogus_maint = ResetRequest {
            workspace_root: &work,
            database_url: "postgres://localhost/main",
            profile: "development",
            confirmed: true,
            allow_checksum_drift_reset: false,
            maintenance_database: "'; DROP DATABASE main; --",
            migrate_config: MigrateConfig::default(),
            audit_pool: None,
            runner_identity: Some(RunnerIdentity::SingleNodeDev),
        };
        match reset_app_database(bogus_maint).await {
            Err(ResetError::InvalidDatabaseName { name }) => {
                assert_eq!(name, "'; DROP DATABASE main; --");
            }
            other => panic!("expected InvalidDatabaseName for maintenance, got {other:?}"),
        }

        // Boundary — a 64-character all-`a` database refuses (over
        // the 63-byte NAMEDATALEN-1 limit).
        let too_long: String = std::iter::repeat_n('a', 64).collect();
        let url = format!("postgres://localhost/{too_long}");
        match reset_app_database(req(&work, &url, "development", true)).await {
            Err(ResetError::InvalidDatabaseName { name }) => assert_eq!(name, too_long),
            other => panic!("expected InvalidDatabaseName, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn replace_db_in_url_round_trips() {
        assert_eq!(
            replace_db_in_url("postgres://localhost/main", "postgres"),
            Some("postgres://localhost/postgres".to_string())
        );
        assert_eq!(
            replace_db_in_url("postgres://user:pass@localhost:5432/main", "postgres"),
            Some("postgres://user:pass@localhost:5432/postgres".to_string())
        );
        // Query string preserved.
        assert_eq!(
            replace_db_in_url("postgresql://localhost/main?sslmode=disable", "postgres"),
            Some("postgresql://localhost/postgres?sslmode=disable".to_string())
        );
    }

    #[test]
    fn quote_identifier_doubles_internal_quotes() {
        assert_eq!(quote_identifier("main"), "\"main\"");
        // Defensive — Postgres URL parsers don't emit quote bytes in
        // database names, but the escape is still correct.
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn scan_committed_migrations_returns_versions_in_lexical_order() {
        use super::super::target::{GLOBAL_BUCKET_DIRNAME, MIGRATIONS_DIR};
        let work = temp_root("scan");
        // Lay down two buckets with two up files each.
        let main_global = work.join(format!("{MIGRATIONS_DIR}/main/{GLOBAL_BUCKET_DIRNAME}"));
        let main_billing = work.join(format!("{MIGRATIONS_DIR}/main/billing"));
        fs::create_dir_all(&main_global).unwrap();
        fs::create_dir_all(&main_billing).unwrap();
        fs::write(
            main_global.join("V20260301000000__init.sdjql"),
            "-- up\nCREATE TABLE foo (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            main_global.join("V20260301000000__init.down.sdjql"),
            "-- down\nDROP TABLE foo;",
        )
        .unwrap();
        fs::write(
            main_global.join("V20260201000000__earlier.sdjql"),
            "-- up\nCREATE TABLE bar (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            main_billing.join("V20260401000000__widgets.sdjql"),
            "-- up\nCREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        // Hand-written `seed.sql` (no `V` prefix) should be skipped.
        fs::write(main_global.join("seed.sql"), "-- not a migration").unwrap();
        // The schema_snapshot.json should be skipped (no `.sdjql`
        // suffix).
        fs::write(main_global.join("schema_snapshot.json"), "{}").unwrap();

        let scanned = scan_committed_migrations(&work, "main").unwrap();
        // Two buckets, in BTreeMap order.
        let mut buckets: Vec<&BucketKey> = scanned.keys().collect();
        buckets.sort();
        assert_eq!(buckets.len(), 2);
        // Global bucket — versions sorted ascending.
        let global_bucket = BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let billing_bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        assert_eq!(
            scanned[&global_bucket],
            vec![
                "V20260201000000__earlier".to_string(),
                "V20260301000000__init".to_string(),
            ]
        );
        assert_eq!(
            scanned[&billing_bucket],
            vec!["V20260401000000__widgets".to_string()]
        );
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn scan_committed_migrations_handles_missing_database_dir() {
        let work = temp_root("missing_db");
        // No migrations/ tree at all → empty map, no error.
        let scanned = scan_committed_migrations(&work, "main").unwrap();
        assert!(scanned.is_empty());
        let _ = fs::remove_dir_all(&work);
    }

    fn historical_entry(
        database: &str,
        app: &str,
        version: &str,
        checksum_up: &str,
        checksum_down: Option<&str>,
    ) -> HistoricalReplayEntry {
        HistoricalReplayEntry {
            bucket: bk(database, app),
            version: version.to_string(),
            status: "applied".to_string(),
            checksum_up: checksum_up.to_string(),
            checksum_down: checksum_down.map(str::to_string),
        }
    }

    #[test]
    fn u275_preflight_checksum_parity_refuses_when_up_sql_drifted() {
        let work = temp_root("u275_up_drift");
        let bucket = bk("main", "");
        let version = "V20260301000000__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();

        let err = preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                version,
                &compute_checksum(["CREATE TABLE widgets (id BIGINT);"]),
                None,
            )],
            false,
        )
        .expect_err("edited up SQL must refuse before destructive reset");

        match err {
            ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
                assert_eq!(issues.len(), 1, "expected one drift issue");
                assert_eq!(issues[0].bucket, bucket);
                assert_eq!(issues[0].version, version);
                assert_eq!(issues[0].sql_side, ResetSqlSide::Up);
                assert_eq!(issues[0].problem, ResetChecksumParityProblem::Drift);
                assert_eq!(
                    issues[0].ledger_checksum,
                    compute_checksum(["CREATE TABLE widgets (id BIGINT);"])
                );
                assert_eq!(
                    issues[0].on_disk_checksum.as_deref(),
                    Some(
                        compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"])
                            .as_str()
                    )
                );
            }
            other => panic!("expected checksum-parity refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_preflight_checksum_parity_refuses_when_down_sql_drifted() {
        let work = temp_root("u275_down_drift");
        let bucket = bk("main", "");
        let version = "V20260301000001__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            "DROP TABLE widgets;",
        )
        .unwrap();

        let err = preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                version,
                &compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"]),
                Some(&compute_checksum(["DROP TABLE widgets CASCADE;"])),
            )],
            false,
        )
        .expect_err("edited down SQL must refuse before destructive reset");

        match err {
            ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
                assert_eq!(issues.len(), 1, "expected one drift issue");
                assert_eq!(issues[0].bucket, bucket);
                assert_eq!(issues[0].version, version);
                assert_eq!(issues[0].sql_side, ResetSqlSide::Down);
                assert_eq!(issues[0].problem, ResetChecksumParityProblem::Drift);
                assert_eq!(
                    issues[0].ledger_checksum,
                    compute_checksum(["DROP TABLE widgets CASCADE;"])
                );
                assert_eq!(
                    issues[0].on_disk_checksum.as_deref(),
                    Some(compute_checksum(["DROP TABLE widgets;"]).as_str())
                );
            }
            other => panic!("expected checksum-parity refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_preflight_checksum_parity_refuses_when_historical_file_is_missing() {
        let work = temp_root("u275_missing_file");
        let err = preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                "V20260301000002__widgets",
                &compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"]),
                None,
            )],
            false,
        )
        .expect_err("missing historical files must refuse before destructive reset");

        match err {
            ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
                assert_eq!(issues.len(), 1, "expected one missing-file issue");
                assert_eq!(issues[0].version, "V20260301000002__widgets");
                assert_eq!(issues[0].sql_side, ResetSqlSide::Up);
                assert_eq!(issues[0].problem, ResetChecksumParityProblem::MissingFile);
                assert!(
                    issues[0].on_disk_checksum.is_none(),
                    "missing file should not claim an on-disk checksum"
                );
            }
            other => panic!("expected checksum-parity refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_preflight_checksum_parity_override_allows_drift() {
        let work = temp_root("u275_override");
        let bucket = bk("main", "");
        let version = "V20260301000003__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();

        preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                version,
                &compute_checksum(["CREATE TABLE widgets (id BIGINT);"]),
                None,
            )],
            true,
        )
        .expect("explicit override should bypass checksum-parity refusal");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_preflight_checksum_parity_refuses_when_down_file_is_missing() {
        let work = temp_root("u275_missing_down");
        let bucket = bk("main", "");
        let version = "V20260301000003__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();

        let err = preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                version,
                &compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"]),
                Some(&compute_checksum(["DROP TABLE widgets;"])),
            )],
            false,
        )
        .expect_err("missing down SQL must refuse before destructive reset");

        match err {
            ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
                assert_eq!(issues.len(), 1, "expected one missing-file issue");
                assert_eq!(issues[0].version, version);
                assert_eq!(issues[0].sql_side, ResetSqlSide::Down);
                assert_eq!(issues[0].problem, ResetChecksumParityProblem::MissingFile);
                assert!(issues[0].on_disk_checksum.is_none());
            }
            other => panic!("expected checksum-parity refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    fn composed_up_sql(version: &str, body: &str) -> String {
        format!(
            "-- Djogi composed migration — up\n\
             -- Version: {version}\n\
             -- Bucket:  main/_global_\n\
             -- Classification: Additive\n\
             -- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n\
             -- AddTable widgets\n\
             {body}\n\n"
        )
    }

    fn composed_down_sql(version: &str, body: &str) -> String {
        format!(
            "-- Djogi composed migration — down\n\
             -- Version: {version}\n\
             -- Bucket:  main/_global_\n\
             -- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n\
             -- DropTable widgets\n\
             {body}\n\n"
        )
    }

    #[test]
    fn u275_preflight_checksum_parity_accepts_composed_sql_headers() {
        let work = temp_root("u275_composed_headers");
        let bucket = bk("main", "");
        let version = "V20260301000012__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            composed_up_sql(version, "CREATE TABLE widgets (id BIGINT PRIMARY KEY);"),
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            composed_down_sql(version, "DROP TABLE widgets;"),
        )
        .unwrap();

        preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                version,
                &compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"]),
                Some(&compute_checksum(["DROP TABLE widgets;"])),
            )],
            false,
        )
        .expect("composed comments and labels must not count as checksum drift");

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_preflight_checksum_parity_refuses_edited_composed_operation_sql() {
        let work = temp_root("u275_composed_operation_drift");
        let bucket = bk("main", "");
        let version = "V20260301000013__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            composed_up_sql(version, "CREATE TABLE widgets (id BIGINT PRIMARY KEY);"),
        )
        .unwrap();

        let err = preflight_reset_checksum_parity(
            &work,
            "main",
            &[historical_entry(
                "main",
                "",
                version,
                &compute_checksum(["CREATE TABLE widgets (id BIGINT);"]),
                None,
            )],
            false,
        )
        .expect_err("operation SQL drift inside composed file must refuse");

        match err {
            ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
                assert_eq!(issues.len(), 1);
                assert_eq!(issues[0].problem, ResetChecksumParityProblem::Drift);
                assert_eq!(
                    issues[0].on_disk_checksum.as_deref(),
                    Some(
                        compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"])
                            .as_str()
                    )
                );
            }
            other => panic!("expected checksum-parity refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_preflight_checksum_parity_refuses_when_baseline_row_cannot_be_compared() {
        let work = temp_root("u275_baseline");
        let bucket = bk("main", "");
        let version = "V20260301000004__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();

        let err = preflight_reset_checksum_parity(
            &work,
            "main",
            &[HistoricalReplayEntry {
                bucket,
                version: version.to_string(),
                status: "baseline".to_string(),
                checksum_up: "V1:baseline-projection".to_string(),
                checksum_down: None,
            }],
            false,
        )
        .expect_err("baseline rows must refuse when reset cannot establish file parity");

        match err {
            ResetError::Refused(ResetRefusal::ChecksumParity { issues }) => {
                assert_eq!(issues.len(), 1, "expected one baseline issue");
                assert_eq!(issues[0].version, version);
                assert_eq!(
                    issues[0].problem,
                    ResetChecksumParityProblem::UnsupportedBaseline
                );
                assert!(issues[0].on_disk_checksum.is_none());
            }
            other => panic!("expected checksum-parity refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_replay_sql_checksums_include_down_when_down_file_exists() {
        let work = temp_root("u275_replay_checksums");
        let bucket = bk("main", "");
        let version = "V20260301000005__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            "DROP TABLE widgets;",
        )
        .unwrap();

        let replay_sql =
            read_replay_sql_files(&work, &bucket, version).expect("load replay SQL files");
        assert_eq!(
            replay_sql.checksum_up,
            compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"])
        );
        assert_eq!(
            replay_sql.checksum_down.as_deref(),
            Some(compute_checksum(["DROP TABLE widgets;"]).as_str()),
            "reset replay must preserve checksum_down so later resets still enforce down-side parity"
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_replay_sql_checksums_treat_comment_only_down_as_none() {
        let work = temp_root("u275_comment_only_down");
        let bucket = bk("main", "");
        let version = "V20260301000016__phase_zero_like";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE SCHEMA IF NOT EXISTS heeranjid;",
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            "-- no meaningful rollback\n-- framework bootstrap is dependency-only\n",
        )
        .unwrap();

        let replay_sql =
            read_replay_sql_files(&work, &bucket, version).expect("load replay SQL files");
        assert!(
            replay_sql.checksum_down.is_none(),
            "comment-only down files must preserve the no-real-rollback null sentinel"
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_preflight_replay_semantics_refuses_non_transactional_sql_without_manifest() {
        let work = temp_root("u276_missing_manifest");
        let bucket = bk("main", "");
        let version = "V20260301000006__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "-- AddTable widgets\n\
             CREATE TABLE widgets (id BIGINT PRIMARY KEY);\n\n\
             -- AddIndex widgets_id_idx\n\
             CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);\n",
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            "DROP INDEX CONCURRENTLY widgets_id_idx;\nDROP TABLE widgets;\n",
        )
        .unwrap();

        let err = preflight_reset_replay_semantics(&work, "main")
            .expect_err("legacy file-only concurrent index replay must refuse before drop");

        match err {
            ResetError::Refused(ResetRefusal::ReplaySemantics { issues }) => {
                assert_eq!(issues.len(), 1, "expected one replay-semantics issue");
                assert_eq!(issues[0].bucket, bucket);
                assert_eq!(issues[0].version, version);
                assert_eq!(
                    issues[0].problem,
                    ResetReplaySemanticsProblem::MissingReplayPlan
                );
                assert_eq!(issues[0].statement_shape, "CREATE INDEX CONCURRENTLY");
            }
            other => panic!("expected replay-semantics refusal, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&work);
    }

    fn assert_single_replay_semantics_issue(
        err: ResetError,
        expected_bucket: &BucketKey,
        expected_version: &str,
        expected_problem: ResetReplaySemanticsProblem,
        expected_shape: &str,
    ) {
        match err {
            ResetError::Refused(ResetRefusal::ReplaySemantics { issues }) => {
                assert_eq!(issues.len(), 1, "expected one replay-semantics issue");
                assert_eq!(issues[0].bucket, *expected_bucket);
                assert_eq!(issues[0].version, expected_version);
                assert_eq!(issues[0].problem, expected_problem);
                assert_eq!(issues[0].statement_shape, expected_shape);
            }
            other => panic!("expected replay-semantics refusal, got {other:?}"),
        }
    }

    #[test]
    fn u276_preflight_replay_semantics_refuses_call_backfill_without_manifest() {
        let work = temp_root("u276_missing_manifest_call");
        let bucket = bk("main", "");
        let version = "V20260301000008__pk_flip_call";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CALL heeranjid_bulk_backfill('widgets', 'id', 'id_desc', 'heer', 10000);\n",
        )
        .unwrap();
        fs::write(bucket_dir.join(down_filename(version)), "SELECT 1;\n").unwrap();

        let err = preflight_reset_replay_semantics(&work, "main")
            .expect_err("CALL backfill replay without manifest must refuse before drop");
        assert_single_replay_semantics_issue(
            err,
            &bucket,
            version,
            ResetReplaySemanticsProblem::MissingReplayPlan,
            "CALL heeranjid_bulk_backfill",
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_preflight_replay_semantics_refuses_do_backfill_without_manifest() {
        let work = temp_root("u276_missing_manifest_do");
        let bucket = bk("main", "");
        let version = "V20260301000009__pk_flip_do";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "DO $$\n\
             BEGIN\n\
                 COMMIT;\n\
             END\n\
             $$;\n",
        )
        .unwrap();
        fs::write(bucket_dir.join(down_filename(version)), "SELECT 1;\n").unwrap();

        let err = preflight_reset_replay_semantics(&work, "main")
            .expect_err("DO backfill replay without manifest must refuse before drop");
        assert_single_replay_semantics_issue(
            err,
            &bucket,
            version,
            ResetReplaySemanticsProblem::MissingReplayPlan,
            "DO block with COMMIT",
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_preflight_replay_semantics_refuses_partitioned_placeholder_without_manifest() {
        let work = temp_root("u276_missing_manifest_partitioned");
        let bucket = bk("main", "");
        let version = "V20260301000010__pk_flip_partitioned";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(
            bucket_dir.join(up_filename(version)),
            "CREATE UNIQUE INDEX events_partition_key_id_desc_idx\n  \
             ON ONLY events (partition_key, id_desc);\n\
             -- Per leaf: CREATE UNIQUE INDEX CONCURRENTLY <leaf>_partition_key_id_desc_idx\n\
             --             ON <leaf> (partition_key, id_desc);\n\
             -- Then ALTER INDEX events_partition_key_id_desc_idx ATTACH PARTITION\n\
             --             <leaf>_partition_key_id_desc_idx;\n",
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            "DROP INDEX IF EXISTS events_partition_key_id_desc_idx;\n",
        )
        .unwrap();

        let err = preflight_reset_replay_semantics(&work, "main")
            .expect_err("partitioned placeholder replay without manifest must refuse before drop");
        assert_single_replay_semantics_issue(
            err,
            &bucket,
            version,
            ResetReplaySemanticsProblem::MissingReplayPlan,
            "PARTITIONED CONCURRENTLY placeholder",
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_load_reset_replay_plan_refuses_invalid_call_manifest() {
        let work = temp_root("u276_invalid_manifest_call");
        let bucket = bk("main", "");
        let version = "V20260301000011__pk_flip_call_invalid";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();
        let up_sql = "CALL heeranjid_bulk_backfill('widgets', 'id', 'id_desc', 'heer', 10000);\n";
        fs::write(bucket_dir.join(up_filename(version)), up_sql).unwrap();
        fs::write(bucket_dir.join(down_filename(version)), "SELECT 1;\n").unwrap();
        fs::write(
            bucket_dir.join(format!("{version}.plan.json")),
            "{\n  \"format_version\": \"1\",\n  \"checksum_up\": \"V1:stale\",\n  \"checksum_down\": null,\n  \"classification\": { \"kind\": \"additive\" },\n  \"segments\": []\n}\n",
        )
        .unwrap();

        let replay_sql =
            read_replay_sql_files(&work, &bucket, version).expect("load replay SQL files");
        let err = load_reset_replay_plan(&work, &bucket, version, &replay_sql)
            .expect_err("invalid non-transactional replay manifest must refuse");
        assert_single_replay_semantics_issue(
            err,
            &bucket,
            version,
            ResetReplaySemanticsProblem::InvalidReplayPlan,
            "CALL heeranjid_bulk_backfill",
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_load_reset_replay_plan_preserves_committed_segment_kinds() {
        let work = temp_root("u276_manifest_roundtrip");
        let bucket = bk("main", "");
        let version = "V20260301000007__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();

        let up_sql = "-- AddTable widgets\n\
             CREATE TABLE widgets (id BIGINT PRIMARY KEY);\n\n\
             -- AddIndex widgets_id_idx\n\
             CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);\n";
        let down_sql = "DROP INDEX CONCURRENTLY widgets_id_idx;\nDROP TABLE widgets;\n";
        fs::write(bucket_dir.join(up_filename(version)), up_sql).unwrap();
        fs::write(bucket_dir.join(down_filename(version)), down_sql).unwrap();
        fs::write(
            bucket_dir.join(format!("{version}.plan.json")),
            format!(
                "{{\n  \"format_version\": \"1\",\n  \"checksum_up\": \"{}\",\n  \"checksum_down\": \"{}\",\n  \"classification\": {{ \"kind\": \"additive\" }},\n  \"segments\": [\n    {{\n      \"kind\": \"transactional\",\n      \"statements\": [\n        {{ \"label\": \"AddTable widgets\", \"up\": \"CREATE TABLE widgets (id BIGINT PRIMARY KEY);\" }}\n      ]\n    }},\n    {{\n      \"kind\": \"non_transactional\",\n      \"statements\": [\n        {{ \"label\": \"AddIndex widgets_id_idx\", \"up\": \"CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);\" }}\n      ]\n    }}\n  ]\n}}\n",
                compute_checksum([up_sql]),
                compute_checksum([down_sql]),
            ),
        )
        .unwrap();

        let replay_sql =
            read_replay_sql_files(&work, &bucket, version).expect("load replay SQL files");
        let plan = load_reset_replay_plan(&work, &bucket, version, &replay_sql)
            .expect("manifest should load")
            .expect("manifest should produce a replay plan");

        assert_eq!(plan.bucket, bucket);
        assert_eq!(
            plan.classification,
            crate::migrate::Classification::Additive
        );
        assert_eq!(plan.segments.len(), 2);
        assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
        assert_eq!(plan.segments[0].statements[0].label, "AddTable widgets");
        assert_eq!(plan.segments[1].kind, SegmentKind::NonTransactional);
        assert_eq!(
            plan.segments[1].statements[0].label,
            "AddIndex widgets_id_idx"
        );

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_load_reset_replay_plan_accepts_composed_sql_operation_checksums() {
        let work = temp_root("u276_manifest_composed_checksum");
        let bucket = bk("main", "");
        let version = "V20260301000014__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();

        fs::write(
            bucket_dir.join(up_filename(version)),
            composed_up_sql(
                version,
                "CREATE TABLE widgets (id BIGINT PRIMARY KEY);\n\n\
                 -- AddIndex widgets_id_idx\n\
                 CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);",
            ),
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            composed_down_sql(
                version,
                "DROP INDEX CONCURRENTLY widgets_id_idx;\nDROP TABLE widgets;",
            ),
        )
        .unwrap();
        fs::write(
            bucket_dir.join(format!("{version}.plan.json")),
            format!(
                "{{\n  \"format_version\": \"1\",\n  \"checksum_up\": \"{}\",\n  \"checksum_down\": \"{}\",\n  \"classification\": {{ \"kind\": \"additive\" }},\n  \"segments\": [\n    {{\n      \"kind\": \"transactional\",\n      \"statements\": [\n        {{ \"label\": \"AddTable widgets\", \"up\": \"CREATE TABLE widgets (id BIGINT PRIMARY KEY);\" }}\n      ]\n    }},\n    {{\n      \"kind\": \"non_transactional\",\n      \"statements\": [\n        {{ \"label\": \"AddIndex widgets_id_idx\", \"up\": \"CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);\" }}\n      ]\n    }}\n  ]\n}}\n",
                compute_checksum([
                    "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
                    "CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);"
                ]),
                compute_checksum(["DROP INDEX CONCURRENTLY widgets_id_idx;\nDROP TABLE widgets;"]),
            ),
        )
        .unwrap();

        let replay_sql =
            read_replay_sql_files(&work, &bucket, version).expect("load replay SQL files");
        assert_eq!(
            replay_sql.checksum_up,
            compute_checksum([
                "CREATE TABLE widgets (id BIGINT PRIMARY KEY);",
                "CREATE INDEX CONCURRENTLY widgets_id_idx ON widgets (id);"
            ])
        );
        let plan = load_reset_replay_plan(&work, &bucket, version, &replay_sql)
            .expect("manifest should load")
            .expect("manifest should produce a replay plan");
        assert_eq!(plan.segments.len(), 2);

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u276_load_reset_replay_plan_accepts_composed_comment_only_down_manifest() {
        let work = temp_root("u276_manifest_comment_only_down");
        let bucket = bk("main", "");
        let version = "V20260301000015__widgets";
        let bucket_dir = super::super::target::bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_dir).unwrap();

        fs::write(
            bucket_dir.join(up_filename(version)),
            composed_up_sql(version, "CREATE TABLE widgets (id BIGINT PRIMARY KEY);"),
        )
        .unwrap();
        fs::write(
            bucket_dir.join(down_filename(version)),
            composed_down_sql(
                version,
                "-- no-op rollback placeholder: lossy operation requires manual rollback",
            ),
        )
        .unwrap();
        fs::write(
            bucket_dir.join(format!("{version}.plan.json")),
            format!(
                "{{\n  \"format_version\": \"1\",\n  \"checksum_up\": \"{}\",\n  \"checksum_down\": null,\n  \"classification\": {{ \"kind\": \"lossy\" }},\n  \"segments\": [\n    {{\n      \"kind\": \"transactional\",\n      \"statements\": [\n        {{ \"label\": \"AddTable widgets\", \"up\": \"CREATE TABLE widgets (id BIGINT PRIMARY KEY);\" }}\n      ]\n    }}\n  ]\n}}\n",
                compute_checksum(["CREATE TABLE widgets (id BIGINT PRIMARY KEY);"]),
            ),
        )
        .unwrap();

        let replay_sql =
            read_replay_sql_files(&work, &bucket, version).expect("load replay SQL files");
        assert!(
            replay_sql.checksum_down.is_none(),
            "composed comment-only down files must preserve compose's checksum_down = null"
        );
        let plan = load_reset_replay_plan(&work, &bucket, version, &replay_sql)
            .expect("manifest should load")
            .expect("manifest should produce a replay plan");
        assert_eq!(plan.segments.len(), 1);

        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn u275_checksum_parity_refusal_display_names_bucket_version_and_checksums() {
        let refusal = ResetRefusal::ChecksumParity {
            issues: vec![
                ResetChecksumParityIssue {
                    bucket: bk("main", ""),
                    version: "V20260301000004__widgets".to_string(),
                    sql_side: ResetSqlSide::Up,
                    ledger_checksum: "V1:ledger".to_string(),
                    on_disk_checksum: Some("V1:disk".to_string()),
                    problem: ResetChecksumParityProblem::Drift,
                },
                ResetChecksumParityIssue {
                    bucket: bk("main", "billing"),
                    version: "V20260301000005__billing".to_string(),
                    sql_side: ResetSqlSide::Down,
                    ledger_checksum: "V1:down-ledger".to_string(),
                    on_disk_checksum: None,
                    problem: ResetChecksumParityProblem::MissingFile,
                },
            ],
        };

        let rendered = refusal.to_string();
        assert!(
            rendered.contains("main/_global_/V20260301000004__widgets"),
            "{rendered}"
        );
        assert!(
            rendered.contains("main/billing/V20260301000005__billing"),
            "{rendered}"
        );
        assert!(rendered.contains("V1:ledger"), "{rendered}");
        assert!(rendered.contains("V1:disk"), "{rendered}");
        assert!(rendered.contains("V1:down-ledger"), "{rendered}");
        assert!(
            rendered.contains("--allow-checksum-drift-reset")
                || rendered.contains("allow_checksum_drift_reset"),
            "{rendered}"
        );
    }

    // ── Historical-order replay plan ──────────────────────────────────

    fn bk(database: &str, app: &str) -> BucketKey {
        BucketKey {
            database: database.to_string(),
            app: app.to_string(),
        }
    }

    /// `build_replay_plan` honours the historical apply order: when
    /// `0001 → applied_at_rank 0`, `0003 → rank 1`, `0002 → rank 2`,
    /// the replay plan is `[0001, 0003, 0002]` — NOT lexical
    /// `[0001, 0002, 0003]`. This is the load-bearing
    /// invariant.
    #[test]
    fn u4_replay_plan_honours_historical_apply_order_over_lexical() {
        let bucket = bk("main", "");
        let mut buckets = BTreeMap::new();
        buckets.insert(
            bucket.clone(),
            vec![
                "V20260101000000__a".to_string(),
                "V20260201000000__b".to_string(),
                "V20260301000000__c".to_string(),
            ],
        );
        // Out-of-order historical apply: a (0), c (1), b (2). Lexical
        // would be a, b, c — different order!
        let mut historical = BTreeMap::new();
        historical.insert((bucket.clone(), "V20260101000000__a".to_string()), 0u64);
        historical.insert((bucket.clone(), "V20260301000000__c".to_string()), 1u64);
        historical.insert((bucket.clone(), "V20260201000000__b".to_string()), 2u64);

        let plan = build_replay_plan(&buckets, &historical);
        let versions: Vec<&str> = plan.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            versions,
            vec![
                "V20260101000000__a",
                "V20260301000000__c",
                "V20260201000000__b",
            ],
            "historical apply order MUST win over lexical version sort"
        );
    }

    /// When NO historical order exists (fresh DB, ledger missing),
    /// the plan falls back to lexical version-string sort. This is
    /// the safe-by-default behaviour that previously reset always
    /// used.
    #[test]
    fn u4_replay_plan_falls_back_to_lexical_when_historical_empty() {
        let bucket = bk("main", "");
        let mut buckets = BTreeMap::new();
        buckets.insert(
            bucket.clone(),
            vec![
                "V20260201000000__b".to_string(),
                "V20260101000000__a".to_string(),
                "V20260301000000__c".to_string(),
            ],
        );
        let historical: BTreeMap<(BucketKey, String), u64> = BTreeMap::new();
        let plan = build_replay_plan(&buckets, &historical);
        let versions: Vec<&str> = plan.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            versions,
            vec![
                "V20260101000000__a",
                "V20260201000000__b",
                "V20260301000000__c",
            ],
            "empty historical map → lexical sort"
        );
    }

    /// Mixed shape: some versions have historical entries, some
    /// don't (typical when files were added on disk after the last
    /// apply). The historical ones come first in apply-order; the
    /// rest sort lexically afterwards.
    #[test]
    fn u4_replay_plan_mixes_historical_and_new_disk_files() {
        let bucket = bk("main", "");
        let mut buckets = BTreeMap::new();
        buckets.insert(
            bucket.clone(),
            vec![
                "V20260101000000__a".to_string(), // historical, rank 0
                "V20260201000000__b".to_string(), // not historical
                "V20260301000000__c".to_string(), // historical, rank 1
                "V20260401000000__d".to_string(), // not historical
            ],
        );
        let mut historical = BTreeMap::new();
        historical.insert((bucket.clone(), "V20260101000000__a".to_string()), 0u64);
        historical.insert((bucket.clone(), "V20260301000000__c".to_string()), 1u64);

        let plan = build_replay_plan(&buckets, &historical);
        let versions: Vec<&str> = plan.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(
            versions,
            vec![
                "V20260101000000__a", // rank 0
                "V20260301000000__c", // rank 1
                "V20260201000000__b", // no rank, lexical first among non-historical
                "V20260401000000__d", // no rank, lexical second
            ],
        );
    }

    // ── Error-policy classification ─────────────────────────────────────

    /// Connecting to a syntactically valid but unreachable URL must
    /// classify as `Transient`, NOT `LedgerMissing`. the
    /// connect-failure path collapsed to an empty map and the
    /// destructive operation would proceed; now the call surfaces
    /// the failure so the caller refuses to drop / recreate.
    /// We point at a port nobody listens on (TCP `:1` is the standard
    /// "discard" pseudo-port — kernels reject the connect immediately).
    #[tokio::test]
    async fn u6_capture_failure_unreachable_url_classifies_as_transient() {
        let url = "postgres://djogi:djogi@127.0.0.1:1/nonexistent_db";
        let res = capture_historical_apply_order(url).await;
        match res {
            Err(HistoricalCaptureError::Transient(e)) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("connect failed")
                        || msg.contains("connect")
                        || msg.contains("Connection")
                        || msg.contains("connection")
                        || msg.contains("refused"),
                    "Transient message must surface the connect failure: {msg}"
                );
            }
            Err(HistoricalCaptureError::LedgerMissing) => {
                panic!(
                    "U-6: connect failure must classify as Transient, NOT LedgerMissing — \
                     pre-fix the unwrap_or_default() collapsed both into the same fallback"
                );
            }
            Ok(_) => panic!("U-6: connect to :1 must fail"),
        }
    }

    /// `ResetError::HistoricalOrderCaptureFailed` is plumbed through
    /// `reset_app_database` end-to-end. We construct a request that
    /// passes every gate (localhost URL with valid identifier name,
    /// development profile, confirmed) but points at an unreachable
    /// port — the historical-order capture's connect step fails and
    /// the variant must propagate.
    /// CRITICAL invariant: pre-fix this same scenario would have
    /// `unwrap_or_default()` ed the failure and proceeded into the
    /// destructive `drop_and_create_database` call. Post-fix the
    /// request returns `HistoricalOrderCaptureFailed` BEFORE any
    /// destructive operation runs.
    #[tokio::test]
    async fn u6_reset_propagates_capture_failure_before_destructive_op() {
        let work = temp_root("u6_capture_propagate");
        // Localhost with a deliberately-wrong port so the gate passes
        // but the connect fails. The is_localhost_connection helper
        // accepts host=127.0.0.1 / host=localhost regardless of port.
        let url = "postgres://djogi:djogi@127.0.0.1:1/main";
        let res = reset_app_database(req(&work, url, "development", true)).await;
        match res {
            Err(ResetError::HistoricalOrderCaptureFailed { source }) => {
                let msg = format!("{source}");
                assert!(
                    msg.contains("connect")
                        || msg.contains("connection")
                        || msg.contains("refused"),
                    "source message must surface the underlying connect failure: {msg}"
                );
            }
            other => panic!(
                "U-6: expected HistoricalOrderCaptureFailed; got {other:?} \
                 (pre-fix this would have proceeded into the destructive drop)"
            ),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// The Display impl for `HistoricalOrderCaptureFailed` carries
    /// operator-actionable language: it names the underlying source,
    /// explains why we refused, and tells the operator what to do
    /// next. This is the message a CI script or human will see when
    /// the gate fires.
    #[test]
    fn u6_historical_order_capture_failed_display_is_actionable() {
        let e = ResetError::HistoricalOrderCaptureFailed {
            source: DjogiError::Db(DbError::other("connection refused".to_string())),
        };
        let s = format!("{e}");
        assert!(s.contains("connection refused"), "must echo source: {s}");
        assert!(
            s.contains("refusing to proceed") || s.contains("refusing"),
            "must explain we refused: {s}"
        );
        assert!(
            s.contains("re-run") || s.contains("rerun"),
            "must guide remediation: {s}"
        );
    }

    /// `ResetError::source()` returns the underlying `DjogiError` for
    /// the this variant — the `?` operator and `tracing` style error
    /// chains depend on that. The pre-existing variants in the impl
    /// already do this; the test guards against forgetting the new
    /// arm if someone touches the match block later.
    #[test]
    fn u6_historical_order_capture_failed_carries_source() {
        use std::error::Error;
        let e = ResetError::HistoricalOrderCaptureFailed {
            source: DjogiError::Db(DbError::other("decode failed".to_string())),
        };
        let src = e.source().expect("must have source");
        assert!(
            src.to_string().contains("decode failed"),
            "source must carry inner message: {}",
            src
        );
    }

    /// Cross-bucket: historical apply ranks impose order across
    /// different buckets too. Without that property, two buckets
    /// applied historically as `bucketA/v1, bucketB/v1, bucketA/v2`
    /// would lose the interleaved ordering.
    #[test]
    fn u4_replay_plan_orders_across_buckets_by_historical_rank() {
        let a = bk("main", "users");
        let b = bk("main", "billing");
        let mut buckets = BTreeMap::new();
        buckets.insert(
            a.clone(),
            vec!["V0001__a1".to_string(), "V0003__a2".to_string()],
        );
        buckets.insert(b.clone(), vec!["V0002__b1".to_string()]);

        // Historical apply: a/V0001 (0), b/V0002 (1), a/V0003 (2).
        let mut historical = BTreeMap::new();
        historical.insert((a.clone(), "V0001__a1".to_string()), 0u64);
        historical.insert((b.clone(), "V0002__b1".to_string()), 1u64);
        historical.insert((a.clone(), "V0003__a2".to_string()), 2u64);

        let plan = build_replay_plan(&buckets, &historical);
        let render: Vec<String> = plan
            .iter()
            .map(|(bucket, v)| format!("{}/{}/{}", bucket.database, bucket.app, v))
            .collect();
        assert_eq!(
            render,
            vec![
                "main/users/V0001__a1".to_string(),
                "main/billing/V0002__b1".to_string(),
                "main/users/V0003__a2".to_string(),
            ],
            "interleaved cross-bucket apply order must be preserved"
        );
    }

    #[test]
    fn reset_discovers_sdjql_migration_files() {
        let root = temp_root("sdjql-discovery");
        let bucket = super::super::target::bucket_dir(&root, &bk("main", "myapp"));
        fs::create_dir_all(&bucket).unwrap();

        fs::write(
            bucket.join("V20260501000000__new.sdjql"),
            "-- Djogi composed migration — up\n-- Version: V20260501000000__new\n\
             -- Bucket:  main/myapp\n-- Classification: Additive\n--\n\
             -- Apply via `djogi migrations apply`, not psql...\n-- DO NOT EDIT...\n\nCREATE TABLE items (id bigint PRIMARY KEY);\n"
        ).unwrap();
        fs::write(
            bucket.join("V20260501000000__new.down.sdjql"),
            "-- Djogi composed migration — down\n-- Version: V20260501000000__new\n\
             -- Bucket:  main/myapp\n-- DO NOT EDIT...\n\nDROP TABLE items;\n",
        )
        .unwrap();

        let scanned =
            super::super::target::scan_filesystem_with_files(&root, Some("main")).unwrap();
        let bk_main = bk("main", "myapp");
        assert!(
            scanned.contains_key(&bk_main),
            "scanner must discover .sdjql files"
        );
        assert_eq!(
            scanned[&bk_main].len(),
            1,
            "should find exactly one up-side file"
        );
        assert!(scanned[&bk_main].contains_key("V20260501000000__new"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reset_rejects_legacy_sql_migration_files() {
        let root = temp_root("legacy-sql-reject");
        let bucket = super::super::target::bucket_dir(&root, &bk("main", "myapp"));
        fs::create_dir_all(&bucket).unwrap();

        fs::write(
            bucket.join("V20260301000000__legacy.sql"),
            "-- Djogi composed migration — up\n-- Version: V20260301000000__legacy\n\
             CREATE TABLE users (id bigint PRIMARY KEY);\n",
        )
        .unwrap();

        let result = super::super::target::scan_filesystem_with_files(&root, Some("main"));
        assert!(
            result.is_err(),
            "reset must reject legacy .sql schema migration files"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("V20260301000000__legacy.sql") || err.contains("legacy"),
            "error must mention the legacy file: {err}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
