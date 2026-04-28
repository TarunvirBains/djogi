//! `db reset` orchestrator — Phase 7 v3 §8 / T8.
//!
//! `db reset` is the destructive triple-gated path: it drops the
//! application database, recreates it, and replays every committed
//! migration found under `migrations/<database>/<app>/`. The triple
//! gate per the v3 §8 brief:
//!
//! 1. `DATABASE_URL` MUST resolve to localhost (reused
//!    [`super::policy::is_localhost_connection`]).
//! 2. `Djogi.toml::profile` MUST NOT equal `"production"`.
//! 3. The caller MUST supply explicit confirmation (a `--yes` flag in
//!    the CLI; programmatic callers pass [`ResetRequest::confirmed`]
//!    `= true`).
//!
//! All three gates are enforced before any I/O. A refusal returns a
//! typed [`ResetError::Refused`] so the operator-facing message is
//! actionable.
//!
//! # Logging-DB isolation
//!
//! Per CLAUDE.md, the CRUD-log and event-log databases survive every
//! `db reset` invocation. Today the runner is single-context (Phase 4)
//! so this module only operates on the application DB; the seam is
//! documented at [`reset_app_database`] for the day the three-database
//! `DjogiContext::pool_for(database)` API lands.
//!
//! # `DROP DATABASE` connection plumbing
//!
//! Postgres refuses to drop the database the current session is
//! connected to. We follow the standard libpq idiom: connect to the
//! `postgres` maintenance database with the same credentials, issue
//! `DROP DATABASE … WITH (FORCE)` (Postgres 13+; we target 18+ per
//! `docs/spec/decisions.md`), then `CREATE DATABASE …`. The forced
//! variant terminates other sessions to avoid the classic "another
//! session is connected" bounce.
//!
//! After recreation the runner re-points at the fresh database via
//! [`crate::pg::pool::DjogiPool::connect`] and replays each migration
//! file pair in HISTORICAL apply order (Codex umbrella U-4 per
//! `docs/spec/configuration.md`). T7's out-of-order policy allows a
//! hotfix migration to apply AFTER a later one, so lexical version
//! sort is NOT a faithful replay of what the live DB experienced.
//! `db reset` pre-flight reads `djogi_schema_migrations.applied_at`
//! BEFORE the drop, then uses that order during replay; versions
//! absent from the historical order (typically disk files added after
//! the last apply) sort lexically afterwards. Fresh DBs with no
//! ledger fall back to lexical sort safely.
//!
//! # Historical-order capture error policy (Codex umbrella round-2 U-6)
//!
//! The pre-flight capture step has TWO qualitatively different
//! failure modes that pre-U-6 collapsed to the same outcome:
//!
//! - **Ledger genuinely missing** (the `pg_class` probe returns
//!   `false`): legitimate fresh-DB fallback. Reset proceeds with an
//!   empty historical map, and `build_replay_plan` falls back to
//!   lexical version sort.
//! - **Anything else** — connection failure, query failure, decode
//!   failure, permission denied: opaque. Reset propagates as
//!   [`ResetError::HistoricalOrderCaptureFailed`] and refuses to
//!   drop / recreate.
//!
//! Pre-U-6 every failure mode swallowed itself via `unwrap_or_default()`
//! at the call site, so a flaky ledger read on a populated DB still
//! triggered the destructive operation. The fix is the
//! [`HistoricalCaptureError`] split: `LedgerMissing` is the only
//! legitimate fall-back signal; `Transient(DjogiError)` propagates.
//!
//! # No regex
//!
//! URL parsing reuses the byte-level extractor in
//! [`super::policy::extract_host`] for the localhost gate, plus a
//! minimal forward-scan helper to split out the `<host>/<dbname>` parts.
//! No regex engine, no regex notation.

// `ResetError` carries an embedded `RunnerError`, which itself embeds
// boxed and string-rich variants; the resulting `Result` payload
// exceeds clippy's default 128-byte threshold for `result_large_err`.
// Boxing the whole error type would force every caller to indirect
// through a heap allocation just to discriminate among the variants —
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

use super::ledger::compute_checksum;
use super::naming::{down_filename, up_filename};
use super::policy::{OutOfOrderPolicy, is_localhost_connection};
use super::projection::BucketKey;
use super::runner::{RunnerCtx, apply_plan};
use super::segment::{MigrationPlan, Segment, SegmentKind};
use super::sql::OperationSql;
use super::target::migrations_root;

// ── Public types ──────────────────────────────────────────────────────────

/// Configuration handed to [`reset_app_database`].
///
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
    /// Maintenance database name. Defaults to `"postgres"` when
    /// the caller has nothing more specific (the conventional
    /// administrative DB present on every cluster).
    pub maintenance_database: &'a str,
    /// Migration-engine config the runner consults during the replay
    /// phase. Operators rarely override this; the CLI default is the
    /// loaded `Djogi.toml::migrate` block.
    pub migrate_config: MigrateConfig,
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
    /// Codex umbrella round-2 U-6: capturing the live ledger's
    /// historical apply order failed for a reason that is NOT
    /// "ledger table is missing on a fresh DB". Pre-fix every
    /// failure mode of the capture step (connection error, decode
    /// error, generic SQL error, …) collapsed to an empty map via
    /// `unwrap_or_default()`, which silently fell through to the
    /// drop / recreate path on a transient error. That re-opens the
    /// U-4 hazard: a flaky ledger read that swallows itself, then
    /// the destructive operation runs anyway against a database
    /// whose true state we never confirmed.
    ///
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
///
/// Triple-gated per the module docs. Returns a [`ResetReport`] on
/// success or a [`ResetError`] on any failure mode — including a
/// gate refusal, which is surfaced as `ResetError::Refused` rather
/// than as a successful no-op.
pub async fn reset_app_database(req: ResetRequest<'_>) -> Result<ResetReport, ResetError> {
    // 1. Triple gate — every gate runs BEFORE any I/O so a refusal
    //    leaves zero side effects on the workspace OR the database.
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

    // 2. Derive the app database name and the maintenance URL.
    //
    //    Two-step parse: first extract+percent-decode the path
    //    component, then validate the decoded bytes against the strict
    //    Postgres-identifier grammar. We refuse weird-looking names
    //    BEFORE splicing them into `DROP DATABASE` / `CREATE DATABASE`
    //    DDL — defence-in-depth against URL-injection where a crafted
    //    URL like `postgres://localhost/'; DROP TABLE foo; --` could
    //    otherwise reach the DDL builder. The maintenance database
    //    name flows from operator config (`--maintenance-database`,
    //    default `postgres`) so it's validated separately.
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
    //    fresh DB; concurrent compose / apply / repair operations
    //    against the same workspace must not interleave with reset.
    let lock_path = req.workspace_root.join(super::guard::LOCK_FILE_NAME);
    let _guard = super::guard::acquire(&lock_path, super::guard::DEFAULT_TIMEOUT)
        .map_err(|e| ResetError::WorkspaceLockFailed { source: e })?;

    // 4. Codex umbrella U-4: capture the HISTORICAL apply order from
    //    the live ledger BEFORE the drop. T7's out-of-order policy
    //    allows a hotfix migration to apply AFTER a later one, e.g.
    //    `applied_at` of `0001 < 0003 < 0002`. Lexical version-string
    //    sort would replay them as `0001, 0002, 0003`, which is NOT
    //    the sequence the live database actually experienced. If
    //    `0002` only succeeded historically because `0003` was
    //    already in place, lexical replay would re-apply it
    //    out-of-order on a fresh DB — different state from what we
    //    just dropped.
    //
    //    Strategy: pre-flight a read-only connection to the live DB,
    //    query `djogi_schema_migrations` ordered by `applied_at`, and
    //    capture `(bucket, version) -> rank`. We then use that rank
    //    as the replay sort key. Versions absent from the historical
    //    order (e.g. files added on disk after the last apply) sort
    //    AFTER any historical entry, lexically among themselves.
    //
    //    Codex umbrella round-2 U-6 — error-policy split:
    //    `HistoricalCaptureError::LedgerMissing` is the ONLY legitimate
    //    fall-back-to-lexical signal (`pg_class` probe returned false:
    //    genuinely fresh DB). Every OTHER failure mode (connection
    //    failure, decode failure, generic SQL error, permission
    //    denied) surfaces as `Transient(..)` and propagates through
    //    `ResetError::HistoricalOrderCaptureFailed`. Pre-U-6 every
    //    error collapsed to `()` and the reset proceeded with an
    //    empty map — which re-opened the U-4 hazard for transient
    //    failures (the empty map masquerades as "fresh DB with no
    //    history" and the destructive drop / recreate runs anyway).
    let historical_order = match capture_historical_apply_order(req.database_url).await {
        Ok(map) => map,
        Err(HistoricalCaptureError::LedgerMissing) => BTreeMap::new(),
        Err(HistoricalCaptureError::Transient(e)) => {
            return Err(ResetError::HistoricalOrderCaptureFailed { source: e });
        }
    };

    // 5. Drop + recreate the application database via the maintenance
    //    connection. A fresh tokio_postgres client is opened just for
    //    the two DDLs — the maintenance pool is intentionally NOT
    //    cached because db reset is interactive / one-shot.
    drop_and_create_database(&maintenance_url, &database).await?;

    // 6. Connect to the freshly-created application DB and replay
    //    every committed migration.
    let pool = DjogiPool::connect(req.database_url)
        .await
        .map_err(|e| ResetError::AppConnectFailed { source: e })?;
    let mut ctx = DjogiContext::from_pool(pool);

    let buckets = scan_committed_migrations(req.workspace_root, &database)?;
    // Codex umbrella U-4: replay order = historical apply order
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
/// per Codex umbrella round-2 U-6.
///
/// The capture step has two qualitatively different failure modes:
///
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
///
/// Pre-U-6 the helper returned `Result<_, ()>` and `unwrap_or_default()`
/// at the call site collapsed every failure mode to "empty map →
/// proceed with lexical fallback". That re-opened the U-4 hazard
/// under a transient connection / query failure: the destructive
/// path runs against a database whose history we never read.
#[derive(Debug)]
enum HistoricalCaptureError {
    /// `pg_class` probe returned `false` — ledger genuinely absent.
    LedgerMissing,
    /// Connection / query / decode failure — treat as opaque, do
    /// NOT proceed with the destructive operation.
    Transient(DjogiError),
}

/// Codex umbrella U-4 + round-2 U-6 — capture the historical apply
/// order from the live ledger before the drop.
///
/// Connects to the application DB at `database_url`, probes for the
/// presence of `djogi_schema_migrations`, and (when present) queries
/// it ordered by `applied_at ASC, id ASC`. Returns a
/// `(bucket, version) -> rank` map where lower ranks applied first
/// historically.
///
/// Per U-6, the error classification is intentional and load-bearing:
///
/// - Probe says ledger absent → `Err(HistoricalCaptureError::LedgerMissing)`
///   (caller falls back to lexical).
/// - Anything else → `Err(HistoricalCaptureError::Transient(..))`
///   (caller propagates and refuses the destructive drop).
///
/// Only `Applied` / `Faked` / `Baseline` rows participate — `Pending`,
/// `Failed`, `RolledBack` do not represent migrations whose effect
/// the live DB carries forward.
async fn capture_historical_apply_order(
    database_url: &str,
) -> Result<BTreeMap<(BucketKey, String), u64>, HistoricalCaptureError> {
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
            "SELECT version, app_label, status \
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

    let mut out: BTreeMap<(BucketKey, String), u64> = BTreeMap::new();
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
        let bucket = BucketKey {
            database: database.clone(),
            app: app_label,
        };
        out.insert((bucket, version), rank as u64);
    }
    drop(client);
    let _ = driver.await;
    Ok(out)
}

/// Codex umbrella U-4 — given the on-disk bucket map and the captured
/// historical apply order, produce the deterministic replay plan as a
/// flat `Vec<(BucketKey, String)>` in the order migrations should be
/// re-applied.
///
/// **Sort key** (lower wins): `(historical_rank.unwrap_or(u64::MAX),
/// bucket.database, bucket.app, version)`. Versions WITH a historical
/// rank apply first (in apply-order); versions WITHOUT (typically
/// disk files added after the last historical apply) apply last,
/// sorted lexically among themselves so re-running the reset
/// produces byte-identical output.
///
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
///
/// Returns a `BTreeMap` so iteration order is deterministic across
/// runs — key order is `(database, app)` ASCII-sorted; per-bucket
/// migration lists are version-sorted (lexical = chronological per
/// the [`super::naming`] convention).
///
/// Files matching the down-side suffix (`.down.sql`) are skipped —
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

/// Read one migration's up SQL, build a single-statement
/// transactional [`MigrationPlan`], and apply it through the runner.
///
/// The replayed plan is a single transactional segment because db
/// reset operates against a fresh database — there are no concurrent
/// writers so the relpages probe + non-transactional split machinery
/// is unnecessary. We trust the on-disk SQL byte-for-byte; the runner
/// still computes its own checksum and verifies it matches what we
/// pre-supplied, so an unexpected file edit during replay surfaces
/// as a typed error.
async fn replay_one_migration(
    ctx: &mut DjogiContext,
    workspace_root: &Path,
    bucket: &BucketKey,
    version: &str,
    migrate_config: &MigrateConfig,
    guard: &super::guard::WorkspaceGuard,
) -> Result<(), ResetError> {
    let bucket_dir = super::target::bucket_dir(workspace_root, bucket);
    let up_path = bucket_dir.join(up_filename(version));
    let down_path = bucket_dir.join(down_filename(version));

    let up_sql = fs::read_to_string(&up_path).map_err(|e| ResetError::SqlReadFailed {
        path: up_path.clone(),
        source: e,
    })?;
    // Down side may not exist for some baseline migrations; treat
    // absence as an empty-string placeholder so the plan still
    // compiles. The replay path itself only runs the up side.
    let down_sql = match fs::read_to_string(&down_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ResetError::SqlReadFailed {
                path: down_path,
                source: e,
            });
        }
    };

    let plan = MigrationPlan {
        bucket: bucket.clone(),
        classification: super::diff::Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: vec![OperationSql {
                label: format!("replay {version}"),
                up: up_sql.clone(),
                down: down_sql,
                lossy: None,
            }],
        }],
    };
    let checksum_up = compute_checksum([up_sql.as_str()]);

    let runner_ctx = RunnerCtx {
        bucket: bucket.clone(),
        version: version.to_string(),
        description: format!("db reset replay of {version}"),
        checksum_up,
        checksum_down: None,
        snapshot: None,
        snapshot_path: None,
        // `MigrateConfig` does not derive `Clone` (the type carries
        // a small fixed-size payload but the wider Phase 7 stance is
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
///
/// Returns `None` when the URL has no path-component database name
/// (e.g. `postgres://localhost`) — `db reset` cannot derive a database
/// to drop in that case.
///
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
///
/// Validation against the Postgres identifier grammar (ASCII letter
/// or underscore followed by ASCII alphanumerics or underscores, up
/// to 63 bytes) is layered on top by [`is_valid_pg_identifier`] —
/// extraction returns the raw decoded string so error messages can
/// surface what the operator actually supplied.
///
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
///
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
///
/// > ASCII letter or underscore, followed by zero-or-more ASCII
/// > alphanumerics or underscores, up to 63 bytes total.
///
/// **No regex.** Byte-level checks per `docs/spec/decisions.md` —
/// `u8::is_ascii_alphabetic`, `u8::is_ascii_alphanumeric`, and
/// explicit byte equality against `b'_'`.
///
/// Postgres' own grammar is technically more permissive (it accepts
/// any byte sequence inside double-quoted identifiers), but the
/// grammar above is the one every Djogi-emitted identifier obeys.
/// Refusing anything wider keeps the `DROP DATABASE` /
/// `CREATE DATABASE` paths free of operator-supplied bytes that the
/// double-quote escape elsewhere in the codebase wouldn't otherwise
/// surface.
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
///
/// Returns `None` when the URL has no recognisable database component.
///
/// Visible to the rest of the crate so the seed runner (Codex B-1)
/// can reuse the same splice — `db seed --database <name>` derives
/// the per-database connection URL from the application URL by
/// replacing the path component in place.
pub(crate) fn replace_db_in_url(url: &str, new_db: &str) -> Option<String> {
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
            maintenance_database: "postgres",
            migrate_config: MigrateConfig::default(),
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

    /// Codex round-1 B-2 — percent-decoding has to happen BEFORE the
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

    /// Codex round-1 B-2 — malformed `%XX` escapes must refuse rather
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

    /// Codex round-1 B-2 — the strict-identifier grammar covers the
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

    /// Codex round-1 B-2 — `reset_app_database` must surface
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
            maintenance_database: "'; DROP DATABASE main; --",
            migrate_config: MigrateConfig::default(),
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
            main_global.join("V20260301000000__init.sql"),
            "-- up\nCREATE TABLE foo (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            main_global.join("V20260301000000__init.down.sql"),
            "-- down\nDROP TABLE foo;",
        )
        .unwrap();
        fs::write(
            main_global.join("V20260201000000__earlier.sql"),
            "-- up\nCREATE TABLE bar (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            main_billing.join("V20260401000000__widgets.sql"),
            "-- up\nCREATE TABLE widgets (id BIGINT PRIMARY KEY);",
        )
        .unwrap();
        // Hand-written `seed.sql` (no `V` prefix) should be skipped.
        fs::write(main_global.join("seed.sql"), "-- not a migration").unwrap();
        // The schema_snapshot.json should be skipped (no `.sql`
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

    // ── Codex umbrella U-4: historical-order replay plan ────────────────

    fn bk(database: &str, app: &str) -> BucketKey {
        BucketKey {
            database: database.to_string(),
            app: app.to_string(),
        }
    }

    /// `build_replay_plan` honours the historical apply order: when
    /// `0001 → applied_at_rank 0`, `0003 → rank 1`, `0002 → rank 2`,
    /// the replay plan is `[0001, 0003, 0002]` — NOT lexical
    /// `[0001, 0002, 0003]`. This is the load-bearing umbrella U-4
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
    /// the safe-by-default behaviour that pre-umbrella reset always
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

    // ── Codex umbrella round-2 U-6: error-policy classification ─────────

    /// Connecting to a syntactically valid but unreachable URL must
    /// classify as `Transient`, NOT `LedgerMissing`. Pre-U-6 the
    /// connect-failure path collapsed to an empty map and the
    /// destructive operation would proceed; post-U-6 the call surfaces
    /// the failure so the caller refuses to drop / recreate.
    ///
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
    ///
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
    /// the U-6 variant — the `?` operator and `tracing` style error
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
}
