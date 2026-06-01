//! Partition / vacuum analysis for adopter Postgres tables.
//! `djogi analyze` (T10) inspects `pg_stat_user_tables` and (when the
//! extension is installed) `pg_partman` metadata to surface vacuum and
//! partitioning recommendations to operators. The recommendation logic
//! ([`recommend`]) is pure — no DB, no I/O, no global state — so it
//! can be unit-tested against synthetic [`TableHealth`] inputs without
//! a live database. The live-DB query path lives in
//! [`fetch_table_health`]; the CLI dispatch entry point is [`run`].
//! # Why a pure substrate
//! `recommend` is exposed as a free function taking only
//! `&TableHealth` plus scalar threshold args. That shape is
//! deliberately deterministic — the same inputs always produce the
//! exact same output bytes. Two consequences fall out:
//! 1. **Byte-stable JSON.** When T10.2 serialises a sorted
//!    `Vec<(table_name, Recommendation)>` to `serde_json`, the result
//!    is reproducible across runs / hosts / Postgres restarts. CI
//!    dashboards that diff yesterday's `analyze --format json` output
//!    against today's see only real changes, never iteration-order
//!    churn.
//! 2. **Trivial unit-testability.** No `tokio` runtime, no fixture DB,
//!    no temp dirs — every recommendation rule is exercised in-process
//!    against hand-built `TableHealth` values.
//! # Threshold rationale
//! Both thresholds are runtime arguments rather than constants because
//! healthy bloat / partition-row ceilings vary per workload. The
//! defaults chosen by the CLI (`0.2` and `10_000_000`) are conservative
//! middle-of-the-road values; OLTP-heavy tables typically tighten the
//! vacuum threshold, while warehouse-style tables loosen the partition
//! row count. Adopters override on the command line without recompiling.

use std::io::Write;

use serde::Serialize;
use tokio_postgres::error::SqlState;

use djogi::__bypass::RawAccessExt as _;
use djogi::DjogiError;
use djogi::config::DjogiConfig;
use djogi::context::DjogiContext;
use djogi::pg::pool::DjogiPool;

/// Snapshot of a single table's vacuum / partition health.
/// Field provenance (per T10.2's planned query):
/// - `table_name` — `pg_stat_user_tables.relname`
/// - `n_live_tup`, `n_dead_tup` — `pg_stat_user_tables` columns of the
///   same name; Postgres-maintained per-row visibility counters.
/// - `last_analyze` — `pg_stat_user_tables.last_analyze`; `None` when
///   the table has never been analysed (e.g. freshly created).
/// - `partition_count` — `0` for plain tables, `>= 1` for partitioned
///   parents (sourced via `pg_partitioned_table` join, with a
///   `pg_partman` fallback when the extension is installed).
///   `last_analyze` is intentionally `time::OffsetDateTime`, not
///   `chrono::DateTime` — djogi forbids `chrono` workspace-wide
///   (CLAUDE.md "Dependencies excluded").
#[derive(Debug, Clone, Serialize)]
pub struct TableHealth {
    pub table_name: String,
    pub n_live_tup: i64,
    pub n_dead_tup: i64,
    pub last_analyze: Option<time::OffsetDateTime>,
    pub partition_count: i32,
}

/// Recommendation produced by [`recommend`] for a single table.
/// # Precedence
/// When multiple rules would fire, [`recommend`] returns the
/// highest-priority match per this strict ordering (highest first):
/// 1. [`Recommendation::VacuumNeeded`] — bloat dominates everything;
///    autovacuum lag is the most operationally urgent signal because
///    dead tuples block index health and inflate disk usage.
/// 2. [`Recommendation::PartitionRecommended`] — an unpartitioned table
///    has crossed the row-count threshold; partitioning is structural
///    work that should land before the table grows further.
/// 3. [`Recommendation::PartitionCountIncrease`] — partitions exist
///    but average row count per partition exceeds the threshold;
///    expanding the partition count is incremental tuning.
/// 4. [`Recommendation::Healthy`] — no rule fires.
/// # JSON shape
/// The `#[serde(tag = "kind", rename_all = "snake_case")]` attribute
/// produces internally-tagged JSON like
/// `{"kind":"vacuum_needed","dead_tup_ratio":0.42}`. T10.2's
/// `--format json` path serialises a sorted vector of
/// `{table, recommendation}` pairs; the snake_case tag keeps the
/// machine-readable output ergonomic for shell scripts and dashboards.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recommendation {
    /// Dead-tuple ratio exceeded `threshold_vacuum`; operator should
    /// run `VACUUM` (or tune autovacuum).
    VacuumNeeded {
        /// `n_dead_tup / (n_live_tup + n_dead_tup)`. Always strictly
        /// greater than the threshold that triggered the variant.
        dead_tup_ratio: f64,
    },
    /// Unpartitioned table whose live row count exceeds
    /// `threshold_partition_rows`; operator should partition.
    PartitionRecommended {
        /// Human-readable explanation including the row count and the
        /// threshold that fired the rule. Stable string format so
        /// `--format json` consumers can grep for substrings (`"not
        /// partitioned"`, `"threshold:"`).
        reason: String,
    },
    /// Partitioned table whose average rows-per-partition exceeds
    /// `threshold_partition_rows`; operator should expand the partition
    /// count.
    PartitionCountIncrease {
        /// Current partition count.
        current: i32,
        /// Suggested partition count — currently a simple doubling.
        /// Bounded by `i32::saturating_mul` so pathological inputs
        /// (e.g. 1.5B partitions) cap at `i32::MAX` rather than
        /// overflowing.
        suggested: i32,
    },
    /// No recommendation — table is within all thresholds.
    Healthy,
}

/// Pure recommendation function for a single table.
/// # Determinism
/// `recommend` takes only the borrowed `TableHealth` and two scalar
/// thresholds. It performs no I/O, reads no globals, allocates only
/// the `String` inside `PartitionRecommended::reason` (when that arm
/// fires), and traverses no unordered collections. Repeated invocation
/// on byte-identical inputs returns byte-identical outputs — the
/// `recommend_is_deterministic` test asserts this with 100 repetitions.
/// # Threshold semantics
/// - `threshold_vacuum`: dead-tuple ratio strictly above which
///   [`Recommendation::VacuumNeeded`] fires. Typical: `0.2` (20% bloat).
///   Higher values mean the operator tolerates more bloat before
///   flagging.
/// - `threshold_partition_rows`: live row count strictly above which
///   an unpartitioned table triggers [`Recommendation::PartitionRecommended`].
///   Typical: `10_000_000`. The same threshold is reused for the
///   per-partition row average that drives
///   [`Recommendation::PartitionCountIncrease`].
/// # Edge cases
/// - Empty table (`n_live_tup == 0 && n_dead_tup == 0`): vacuum check
///   is short-circuited (division-by-zero guard). Partition checks
///   still run but neither fires for an empty table.
/// - `partition_count == 0`: treated as "not partitioned" — only the
///   `PartitionRecommended` rule can fire.
/// - `partition_count >= 1` but row count below threshold: falls
///   through to `Healthy`.
/// # See also
/// [`Recommendation`] for the precedence ordering.
pub fn recommend(
    health: &TableHealth,
    threshold_vacuum: f64,
    threshold_partition_rows: i64,
) -> Recommendation {
    // 1. VacuumNeeded — highest priority. Skipped on empty tables to
    // avoid 0/0; an empty table cannot be bloated by definition.
    // `saturating_add` caps at `i64::MAX` rather than panicking
    // (debug) or wrapping (release) when both counters approach
    // `i64::MAX` — pathological stats values still produce a valid
    // ratio in `[0.0, 1.0]`.
    let total_tup = health.n_live_tup.saturating_add(health.n_dead_tup);
    if total_tup > 0 {
        let ratio = health.n_dead_tup as f64 / total_tup as f64;
        if ratio > threshold_vacuum {
            return Recommendation::VacuumNeeded {
                dead_tup_ratio: ratio,
            };
        }
    }

    // 2. PartitionRecommended — unpartitioned table over the row
    // threshold. `partition_count == 0` is the unpartitioned signal.
    if health.partition_count == 0 && health.n_live_tup > threshold_partition_rows {
        return Recommendation::PartitionRecommended {
            reason: format!(
                "table has {} live rows but is not partitioned (threshold: {})",
                health.n_live_tup, threshold_partition_rows
            ),
        };
    }

    // 3. PartitionCountIncrease — partitioned but undersized partitions.
    // Average is integer-divided; precision is irrelevant since the
    // threshold gate is also an integer comparison.
    if health.partition_count > 0 {
        let avg_per_partition = health.n_live_tup / health.partition_count as i64;
        if avg_per_partition > threshold_partition_rows {
            return Recommendation::PartitionCountIncrease {
                current: health.partition_count,
                // Saturating multiplication caps at `i32::MAX` rather
                // than overflowing — pathological partition counts
                // (e.g. > i32::MAX/2) still produce a valid suggestion.
                suggested: health.partition_count.saturating_mul(2),
            };
        }
    }

    // 4. Healthy — no rule fired.
    Recommendation::Healthy
}

/// Errors surfaced by [`run`] / [`fetch_table_health`].
/// Each variant carries operator-actionable context — the goal is that
/// an `eprintln!("djogi analyze: {e}")` line is enough to diagnose
/// without grepping source. Mirrors `verify::VerifyError`'s shape.
#[derive(Debug)]
pub enum AnalyzeError {
    /// Loading `Djogi.toml` (and its env overlays) failed.
    Config(String),
    /// Could not connect to the application database. URL is included
    /// for diagnostics; the underlying `DjogiError` is rendered into
    /// `message` so `AnalyzeError` stays `Send + Sync` regardless of
    /// the variant we received.
    Pool {
        /// The application DB URL we attempted to reach. Echoed in the
        /// operator-facing message so the resolution path is visible.
        url: String,
        /// Underlying error message. The framework's
        /// `DjogiError::Pool` enum carries trait-object inner values
        /// that aren't always `Send + Sync`; rendering eagerly keeps
        /// `AnalyzeError` cheap to clone in test assertions and
        /// avoids leaking deadpool types to consumers.
        message: String,
    },
    /// `pg_stat_user_tables` query (or the optional pg_partman join)
    /// surfaced a Postgres error that is NOT one of the "extension
    /// absent" SQLSTATEs we tolerate. Rendered eagerly for the same
    /// reasons as `Pool::message`.
    Db(String),
    /// Writing the rendered output to stdout failed (broken pipe,
    /// disk full on a redirected `> file.json`, etc.).
    Io(std::io::Error),
    /// `serde_json` encoding failed. Should be unreachable because the
    /// `Row` projection only contains primitive / `OffsetDateTime` /
    /// known-`Serialize` types, but we surface it as a structured
    /// variant rather than `unwrap` so a future schema extension
    /// that introduces a non-serialisable field fails loudly.
    Json(serde_json::Error),
}

impl std::fmt::Display for AnalyzeError {
    /// Every variant ships an operator-actionable remediation hint after
    /// the cause — `eprintln!("djogi analyze: {e}")` should be enough to
    /// either fix the problem or know what to file. The shape is
    /// "<cause>. <remediation verb>...". The
    /// `analyze_error_display_is_operator_actionable` test pins each
    /// variant's remediation keyword (`Verify` / `Check` / `file an
    /// issue`) so a future drift trips the test rather than silently
    /// losing the hint.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeError::Config(message) => write!(
                f,
                "config load: {message}. \
                 Verify the Djogi.toml workspace path and the [database] section.",
            ),
            AnalyzeError::Pool { url, message } => write!(
                f,
                "application DB at `{url}` unreachable: {message}. \
                 Verify Djogi.toml::database.url is reachable and the \
                 credentials grant CONNECT.",
            ),
            AnalyzeError::Db(message) => write!(
                f,
                "live-DB query: {message}. \
                 Verify the app DB is reachable and the role has SELECT \
                 privilege on pg_stat_user_tables (and on partman.* if \
                 pg_partman is installed).",
            ),
            AnalyzeError::Io(e) => write!(
                f,
                "writing analyze output: {e}. \
                 Check stdout/stderr permissions and the workspace path.",
            ),
            AnalyzeError::Json(e) => write!(
                f,
                "encoding analyze output as JSON: {e}. \
                 This is an internal bug — please file an issue with the \
                 input that triggered it.",
            ),
        }
    }
}

impl std::error::Error for AnalyzeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AnalyzeError::Io(e) => Some(e),
            AnalyzeError::Json(e) => Some(e),
            AnalyzeError::Config(_) | AnalyzeError::Pool { .. } | AnalyzeError::Db(_) => None,
        }
    }
}

impl From<std::io::Error> for AnalyzeError {
    fn from(e: std::io::Error) -> Self {
        AnalyzeError::Io(e)
    }
}

impl From<serde_json::Error> for AnalyzeError {
    fn from(e: serde_json::Error) -> Self {
        AnalyzeError::Json(e)
    }
}

/// Output format selector — wired up to the CLI's `--format` flag.
/// `Human` is for direct operator reading; `Json` is for CI dashboards
/// and other machine consumers. Both paths emit deterministic output
/// (sorted by `table_name`, no `HashMap` iteration anywhere on the
/// rendering path) so a `diff` between yesterday's and today's run
/// shows only real schema-health changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeFormat {
    /// One sorted ASCII line per table — `table | live | dead | parts | recommendation`.
    Human,
    /// Pretty-printed JSON array of `{table_name, recommendation, ...}`
    /// rows, sorted by `table_name`. Pretty rather than compact so
    /// `git diff` between dashboard runs is reviewable.
    Json,
}

/// Pull live-DB stats from `pg_stat_user_tables` and (optionally)
/// `pg_partman.show_partitions(...)` for every user table.
/// # Query design
/// The primary query lists schema-qualified table names plus visibility
/// counters and last-analyze timestamps from `pg_stat_user_tables`.
/// That catalogue is part of every Postgres install, so this query is
/// always available and never errors with `UNDEFINED_TABLE`.
/// The per-row partition lookup uses `partman.show_partitions($1)`
/// which is part of the optional `pg_partman` extension. Many adopters
/// will not have it installed; rather than refusing to run analyze on
/// those clusters, we catch SQLSTATE `UNDEFINED_FUNCTION` (`42883`),
/// `UNDEFINED_TABLE` (`42P01`), and `INVALID_SCHEMA_NAME` (`3F000`)
/// from the partman call and fall back to `partition_count = 0`. The
/// fallback collapses the partman path to "no partitions reported"
/// rather than producing a hard error — which is the right semantic
/// because a table that pg_partman doesn't know about is, from
/// analyze's perspective, indistinguishable from an unpartitioned
/// table.
/// # Determinism
/// `ORDER BY table_name` in the primary query plus a defensive
/// `Vec::sort_by` after collection (in [`run`]) means the output
/// ordering does not depend on Postgres planner decisions.
/// # Read-only
/// Both queries are `SELECT`-only with positional binds. No DDL, no
/// DML — analyze never writes.
pub async fn fetch_table_health(pool: &DjogiPool) -> Result<Vec<TableHealth>, AnalyzeError> {
    let mut ctx = DjogiContext::from_pool(pool.clone());

    // Probe `partman` schema AND `show_partitions` function existence
    // ONCE up-front rather than letting every per-table
    // `partman.show_partitions(...)` call discover the absence at
    // prepare time.
    // # Why probe up-front
    // `prepare_cached` (the path `raw_rows` routes through) maps
    // tokio-postgres errors via `DbError::other(e.to_string)`
    // which DROPS the SQLSTATE because the prepare-error path
    // collapses the `tokio_postgres::Error` into a message-only
    // `DbError` whose `code` returns `None`. The original
    // `is_partman_absent_code` SQLSTATE classifier therefore never
    // matches against a cluster without pg_partman, and every
    // analyze run on such a cluster fails with `AnalyzeError::Db`
    // even though the partman call should be a soft fallback.
    // # Why probe BOTH schema and function
    // A schema-only check (`pg_namespace.nspname = 'partman'`) is
    // insufficient: a partial install (e.g. the extension was
    // dropped but the schema was preserved by a CASCADE-less
    // `DROP EXTENSION pg_partman`, or a mid-upgrade where the
    // schema exists but the new function shape has not been
    // installed yet) leaves the schema present without
    // `show_partitions`. The per-table call would then `prepare`
    // the function reference, fail with `UNDEFINED_FUNCTION`, and
    // hit the same SQLSTATE-dropping path described above — so the
    // soft fallback would never engage and analyze would fail.
    // Joining `pg_proc` against `pg_namespace.oid = pg_proc.pronamespace`
    // confirms the function we're about to call actually exists in
    // the partman schema. The probe returns true only when BOTH the
    // schema and `show_partitions` are present; any partial-install
    // shape returns false and we skip the per-table partman calls
    // entirely (`partition_count = 0` for every row — the same
    // outcome the SQLSTATE-fallback path would have produced for an
    // entirely-absent extension).
    // Both `pg_namespace` and `pg_proc` are core system catalogues
    // the probe is always-prepareable, returns a clean
    // `Result<bool, _>`, and costs ONE query per analyze invocation
    // instead of N (one per user table).
    // The retained `query_partition_count` SQLSTATE classifier is a
    // belt-and-braces guard for execution-time partman failures: if
    // `show_partitions` exists at probe time but errors at runtime
    // (e.g. permission revoked between probe and call, or the
    // function body itself raises `UNDEFINED_TABLE` because no
    // partitioned parents are registered), we still soft-degrade
    // to `partition_count = 0`. That path goes through `query` not
    // `prepare`, so the SQLSTATE survives and the classifier still
    // catches it.
    let partman_present = !ctx
        .raw_rows(
            "SELECT 1 \
             FROM pg_namespace n \
             JOIN pg_proc p ON p.pronamespace = n.oid \
             WHERE n.nspname = 'partman' AND p.proname = 'show_partitions'",
            &[],
        )
        .await
        .map_err(djogi_err_to_analyze)?
        .is_empty();

    // Primary query — `pg_stat_user_tables` is always available on
    // every Postgres cluster djogi targets (>= 18). The schema name is
    // joined into `table_name` so partitioned tables in non-public
    // schemas show up unambiguously in the operator output.
    let stats_sql = "SELECT \
                     schemaname || '.' || relname AS table_name, \
                     n_live_tup, \
                     n_dead_tup, \
                     last_analyze \
                     FROM pg_stat_user_tables \
                     ORDER BY schemaname, relname";
    let stats_rows = ctx
        .raw_rows(stats_sql, &[])
        .await
        .map_err(djogi_err_to_analyze)?;

    // Per-row partition lookup. Errors of the "extension absent" class
    // are swallowed into `partition_count = 0`; everything else
    // propagates as `AnalyzeError::Db`. When the up-front probe says
    // partman is absent, we skip the per-table query entirely and
    // assign `partition_count = 0` directly — same outcome, no
    // per-table round-trip.
    let mut out = Vec::with_capacity(stats_rows.len());
    for row in stats_rows {
        // `try_get` (not `get`) per the `raw_rows` contract: the row
        // accessor that returns `Result<T, _>` rather than panicking on
        // a type/decode mismatch. A future Postgres upgrade or a
        // restrictive role granting access to `pg_stat_user_tables` with
        // unexpected nullability could otherwise crash the CLI; routing
        // through `AnalyzeError::Db` keeps every failure path
        // operator-actionable.
        let table_name: String = row
            .try_get(0)
            .map_err(|e| AnalyzeError::Db(format!("decoding table_name: {e}")))?;
        let n_live_tup: i64 = row
            .try_get(1)
            .map_err(|e| AnalyzeError::Db(format!("decoding n_live_tup: {e}")))?;
        let n_dead_tup: i64 = row
            .try_get(2)
            .map_err(|e| AnalyzeError::Db(format!("decoding n_dead_tup: {e}")))?;
        let last_analyze: Option<time::OffsetDateTime> = row
            .try_get(3)
            .map_err(|e| AnalyzeError::Db(format!("decoding last_analyze: {e}")))?;

        let partition_count = if partman_present {
            match query_partition_count(&mut ctx, &table_name).await {
                Ok(count) => count,
                Err(PartmanError::Absent) => 0,
                Err(PartmanError::Other(message)) => return Err(AnalyzeError::Db(message)),
            }
        } else {
            0
        };

        out.push(TableHealth {
            table_name,
            n_live_tup,
            n_dead_tup,
            last_analyze,
            partition_count,
        });
    }

    Ok(out)
}

/// Internal error type for [`query_partition_count`]. Distinct from
/// [`AnalyzeError`] so the caller can match on `Absent` (treat as
/// `partition_count = 0`) without conflating with the broader
/// `AnalyzeError::Db` channel.
enum PartmanError {
    /// `pg_partman` is not installed on this cluster — the
    /// `partman.show_partitions` function or `partman` schema is
    /// undefined. Treated as "no partitions" by the caller.
    Absent,
    /// Anything else — connection error, syntax error, permission
    /// denied. Rendered into `AnalyzeError::Db`.
    Other(String),
}

/// Query the partition count for `table_name` via
/// `partman.show_partitions($1)`.
/// **Parameter binding.** `$1` carries `table_name`; we never
/// `format!`-interpolate. The table name comes from
/// `pg_stat_user_tables` (a system catalogue, so trusted), but the
/// codebase rule is "always parameterise" — there is no scenario in
/// which the cost of a parameter bind matters, and a stray code path
/// that builds the table name from an untrusted source later cannot
/// regress this query into an injection vector.
/// Returns `Err(PartmanError::Absent)` for the three SQLSTATEs that
/// indicate "pg_partman not installed":
/// - `42883` `UNDEFINED_FUNCTION` — `partman` schema present but
///   `show_partitions` not (e.g. partial install).
/// - `42P01` `UNDEFINED_TABLE` — `show_partitions` resolves but the
///   underlying `part_config` lookup fails because no partitioned
///   parents are registered.
/// - `3F000` `INVALID_SCHEMA_NAME` — `partman` schema entirely
///   absent.
async fn query_partition_count(
    ctx: &mut DjogiContext,
    table_name: &str,
) -> Result<i32, PartmanError> {
    let sql = "SELECT count(*)::int FROM partman.show_partitions($1)";
    match ctx.raw_rows(sql, &[&table_name]).await {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                // `try_get` (not `get`) per the `raw_rows` contract — a
                // future change to `partman.show_partitions` return shape
                // would otherwise panic the CLI instead of surfacing as
                // a structured `Other` variant the caller routes into
                // `AnalyzeError::Db`.
                let count: i32 = row
                    .try_get(0)
                    .map_err(|e| PartmanError::Other(format!("decoding partition count: {e}")))?;
                Ok(count)
            } else {
                // `count(*)` always returns one row, but defend against
                // a future Postgres upgrade that could change the
                // contract — reporting "0 partitions" is the right
                // fallback if the row is somehow missing.
                Ok(0)
            }
        }
        Err(DjogiError::Db(db)) => {
            if let Some(code) = db.code()
                && is_partman_absent_code(code)
            {
                Err(PartmanError::Absent)
            } else {
                Err(PartmanError::Other(db.to_string()))
            }
        }
        Err(other) => Err(PartmanError::Other(other.to_string())),
    }
}

/// Map the three SQLSTATE classes that signal "pg_partman not
/// installed" onto a single boolean. Centralised so both
/// [`query_partition_count`] and any future caller reuse the same
/// classification.
fn is_partman_absent_code(code: &SqlState) -> bool {
    *code == SqlState::UNDEFINED_FUNCTION
        || *code == SqlState::UNDEFINED_TABLE
        || *code == SqlState::INVALID_SCHEMA_NAME
}

/// Convert a `DjogiError` from the live-DB path into an
/// `AnalyzeError::Db`. Rendered eagerly (`to_string`) so the
/// `AnalyzeError` does not need to carry the heterogeneous inner type.
fn djogi_err_to_analyze(e: DjogiError) -> AnalyzeError {
    AnalyzeError::Db(e.to_string())
}

/// `djogi analyze` entry point — consumed by `main.rs::TopCommand::Analyze`.
/// Orchestrates the live-DB pull, the pure recommendation pass, and
/// the rendering. Splitting fetch / recommend / render this way means
/// every test (unit, integration, regression) targets exactly the
/// layer that interests it without dragging in the others.
/// # Workspace + config resolution
/// `workspace` is `None` by default — we resolve to
/// `std::env::current_dir` and then load `Djogi.toml` via
/// `DjogiConfig::load_from_workspace`. Mirrors `verify::run`'s pattern.
/// # Pool lifecycle
/// One pool, one context, every per-table query runs through it.
/// Built fresh on every invocation — analyze is a one-shot CLI command,
/// not a long-lived process, so pool reuse across invocations is not a
/// goal.
/// # Output destination
/// Both rendering paths write to a locked `stdout`. Locking once at
/// the top means we don't pay the `Stdout::lock` cost per row, and
/// the renderers themselves take a generic `&mut W: Write` so the
/// pure render-only tests (T10.2's `render_human_*` / `render_json_*`)
/// can target a `Vec<u8>` without going through stdout.
pub async fn run(
    workspace: Option<std::path::PathBuf>,
    format: AnalyzeFormat,
    threshold_vacuum: f64,
    threshold_partition_rows: i64,
) -> Result<(), AnalyzeError> {
    // Step 1 — resolve workspace, load config.
    let workspace = workspace.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });
    let config = DjogiConfig::load_from_workspace(&workspace)
        .map_err(|e| AnalyzeError::Config(e.to_string()))?;

    // Step 2 — connect to the application DB.
    let url = config.database.url.clone();
    let pool = DjogiPool::connect(&url)
        .await
        .map_err(|e| AnalyzeError::Pool {
            url: url.clone(),
            message: e.to_string(),
        })?;
    djogi::pg::preflight::check_postgres_version(&pool)
        .await
        .map_err(|e| AnalyzeError::Pool {
            url: url.clone(),
            message: format!("support boundary: {e}"),
        })?;

    // Step 3 — fetch + recommend.
    let mut health = fetch_table_health(&pool).await?;
    // Defence-in-depth — the SQL `ORDER BY` already sorts the rows,
    // but we re-sort by the materialised `table_name` so the
    // determinism contract is independent of the planner.
    health.sort_by(|a, b| a.table_name.cmp(&b.table_name));

    let report: Vec<(TableHealth, Recommendation)> = health
        .into_iter()
        .map(|h| {
            let rec = recommend(&h, threshold_vacuum, threshold_partition_rows);
            (h, rec)
        })
        .collect();

    // Step 4 — render. Lock stdout once so the renderers don't pay the
    // per-row lock cost and a downstream `tee` / pipe sees one
    // contiguous stream.
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match format {
        AnalyzeFormat::Human => render_human(&report, &mut handle)?,
        AnalyzeFormat::Json => render_json(&report, &mut handle)?,
    }
    handle.flush()?;
    Ok(())
}

/// Render the human-readable ASCII table.
/// One header line plus one body line per table. Columns are padded so
/// the visual alignment is stable across runs; the recommendation
/// column carries a short tag (`vacuum`, `partition`, `parts++`,
/// `healthy`) plus the structural detail. Empty input prints the
/// header only — no "no rows" placeholder, so a downstream `wc -l`
/// sees the row count directly.
/// # Determinism
/// The input is already sorted by `table_name` (see [`run`]); this
/// renderer iterates in the input order. No `HashMap`, no
/// `BTreeMap` — pure `Vec` iteration.
fn render_human<W: Write>(
    report: &[(TableHealth, Recommendation)],
    out: &mut W,
) -> Result<(), AnalyzeError> {
    // Header — clippy's `write_literal` lint flags trailing
    // `"<lit>"` arguments paired with `{}` placeholders, so we keep
    // the format string fully width-specified and stage the header
    // labels through bindings rather than literal slots.
    let h_table = "TABLE";
    let h_live = "LIVE";
    let h_dead = "DEAD";
    let h_parts = "PARTITIONS";
    let h_rec = "RECOMMENDATION";
    writeln!(
        out,
        "{h_table:<48} {h_live:>14} {h_dead:>14} {h_parts:>10}  {h_rec}",
    )?;
    for (h, r) in report {
        writeln!(
            out,
            "{:<48} {:>14} {:>14} {:>10}  {}",
            h.table_name,
            h.n_live_tup,
            h.n_dead_tup,
            h.partition_count,
            recommendation_human(r),
        )?;
    }
    Ok(())
}

/// Render a single recommendation as a one-line ASCII tag for the
/// human renderer. Stable text format — string-substring assertions on
/// the operator output are part of the contract.
fn recommendation_human(r: &Recommendation) -> String {
    match r {
        Recommendation::VacuumNeeded { dead_tup_ratio } => {
            format!("vacuum (dead_tup_ratio={dead_tup_ratio:.4})")
        }
        Recommendation::PartitionRecommended { reason } => {
            format!("partition ({reason})")
        }
        Recommendation::PartitionCountIncrease { current, suggested } => {
            format!("parts++ (current={current}, suggested={suggested})")
        }
        Recommendation::Healthy => "healthy".to_string(),
    }
}

/// Render the JSON array.
/// # Determinism contract (, T10.1 review)
/// We project through a named `Row` struct rather than a
/// `HashMap<String, _>` — `serde`'s default behaviour preserves struct
/// field declaration order, so the JSON output is byte-stable across
/// runs. `serde_json::to_writer_pretty` is chosen over the compact
/// form so dashboards can `git diff` two runs without reformatting
/// first.
/// # Field order
/// `table_name` first (sort key), then the raw counters
/// (`n_live_tup`, `n_dead_tup`), then `last_analyze`,
/// `partition_count`, and finally the structured `recommendation`.
/// Pinned by `render_json_field_order_is_stable`.
fn render_json<W: Write>(
    report: &[(TableHealth, Recommendation)],
    out: &mut W,
) -> Result<(), AnalyzeError> {
    /// Wire-format projection of one analyzed table.
    /// Field declaration order IS the JSON field order — see the
    /// renderer's determinism contract above. Borrows from the
    /// in-memory `report` so the projection is allocation-free.
    #[derive(Serialize)]
    struct Row<'a> {
        table_name: &'a str,
        n_live_tup: i64,
        n_dead_tup: i64,
        last_analyze: Option<time::OffsetDateTime>,
        partition_count: i32,
        recommendation: &'a Recommendation,
    }

    let rows: Vec<Row<'_>> = report
        .iter()
        .map(|(h, r)| Row {
            table_name: &h.table_name,
            n_live_tup: h.n_live_tup,
            n_dead_tup: h.n_dead_tup,
            last_analyze: h.last_analyze,
            partition_count: h.partition_count,
            recommendation: r,
        })
        .collect();

    serde_json::to_writer_pretty(&mut *out, &rows)?;
    // serde_json::to_writer_pretty does NOT emit a trailing newline;
    // adding one keeps the output well-behaved for shell pipelines
    // (e.g. `djogi analyze --format json | jq .`).
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Pure unit tests covering every arm of `Recommendation` plus
    //! precedence ordering and determinism. None of these tests touch
    //! the network, the filesystem, or a database — they construct
    //! `TableHealth` values directly and assert on the returned
    //! `Recommendation`.

    use super::*;

    /// Helper: build a `TableHealth` with sensible defaults so tests
    /// only override the fields they care about. Centralising the
    /// builder keeps test bodies focused on the rule being exercised.
    fn health(n_live_tup: i64, n_dead_tup: i64, partition_count: i32) -> TableHealth {
        TableHealth {
            table_name: "test_table".to_string(),
            n_live_tup,
            n_dead_tup,
            last_analyze: None,
            partition_count,
        }
    }

    #[test]
    fn recommend_healthy_when_below_all_thresholds() {
        // Small table, no dead tuples, plenty of partition headroom.
        let h = health(1_000, 0, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Same idea but partitioned and well below the per-partition
        // ceiling — also Healthy.
        let h = health(100_000, 0, 4);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Genuinely empty table — Healthy (vacuum guard skips the
        // division, partition checks don't fire below threshold).
        let h = health(0, 0, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);
    }

    #[test]
    fn recommend_vacuum_when_dead_tup_ratio_high() {
        // Just above 0.2 — fires.
        // 21 dead / (79 live + 21 dead) = 0.21
        let h = health(79, 21, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                assert!(
                    dead_tup_ratio > 0.20 && dead_tup_ratio < 0.22,
                    "expected ratio near 0.21, got {dead_tup_ratio}"
                );
            }
            other => panic!("expected VacuumNeeded, got {other:?}"),
        }

        // Just below 0.2 — does NOT fire.
        // 19 dead / 100 = 0.19
        let h = health(81, 19, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Exactly at 0.2 — does NOT fire (strict greater-than).
        let h = health(80, 20, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // High ratio (50%) — fires unambiguously.
        let h = health(50, 50, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                assert!((dead_tup_ratio - 0.5).abs() < 1e-9);
            }
            other => panic!("expected VacuumNeeded, got {other:?}"),
        }
    }

    #[test]
    fn recommend_partition_when_unpartitioned_and_large() {
        // Just above 10M rows, no dead tuples, no partitions.
        let h = health(10_000_001, 0, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::PartitionRecommended { reason } => {
                assert!(reason.contains("10000001"), "reason: {reason}");
                assert!(reason.contains("not partitioned"), "reason: {reason}");
                assert!(reason.contains("threshold: 10000000"), "reason: {reason}");
            }
            other => panic!("expected PartitionRecommended, got {other:?}"),
        }

        // Exactly at threshold — does NOT fire (strict greater-than).
        let h = health(10_000_000, 0, 0);
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);

        // Way over threshold but already partitioned — does NOT fire
        // the unpartitioned rule (CountIncrease may, see below).
        let h = health(20_000_000, 0, 100);
        // 20M / 100 = 200k average → below 10M threshold → Healthy.
        assert_eq!(recommend(&h, 0.2, 10_000_000), Recommendation::Healthy);
    }

    #[test]
    fn recommend_partition_count_increase_when_partitions_undersized() {
        // 100M rows across 4 partitions = 25M each → exceeds 10M.
        let h = health(100_000_000, 0, 4);
        assert_eq!(
            recommend(&h, 0.2, 10_000_000),
            Recommendation::PartitionCountIncrease {
                current: 4,
                suggested: 8,
            }
        );

        // Saturating-mul guard: even pathological partition counts
        // produce a valid `i32` suggestion.
        let h = health(i64::MAX / 2, 0, i32::MAX);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::PartitionCountIncrease { current, suggested } => {
                assert_eq!(current, i32::MAX);
                assert_eq!(suggested, i32::MAX); // saturated
            }
            other => panic!("expected PartitionCountIncrease, got {other:?}"),
        }
    }

    #[test]
    fn recommend_is_deterministic() {
        // Build a single TableHealth and run recommend 100 times;
        // every result must equal the first. Covers the
        // concern about HashMap-iteration nondeterminism — there is no
        // HashMap in `recommend`, but the test cements the contract so
        // future refactors don't sneak one in.
        let h = health(50_000_000, 0, 3);
        let baseline = recommend(&h, 0.2, 10_000_000);

        for i in 0..100 {
            let result = recommend(&h, 0.2, 10_000_000);
            assert_eq!(
                result, baseline,
                "iteration {i} diverged from baseline {baseline:?}"
            );
        }

        // Same shape with the VacuumNeeded arm — float math should
        // also be bit-stable across repeated invocations on the same
        // inputs.
        let h = health(70, 30, 0);
        let baseline = recommend(&h, 0.2, 10_000_000);
        for i in 0..100 {
            assert_eq!(
                recommend(&h, 0.2, 10_000_000),
                baseline,
                "vacuum iteration {i} diverged"
            );
        }
    }

    #[test]
    fn recommend_vacuum_dominates_partition() {
        // Both VacuumNeeded AND PartitionRecommended would fire in
        // isolation — vacuum wins per precedence ordering.
        // Setup: 100M live + 50M dead → 33% dead ratio AND
        // unpartitioned over the 10M row threshold.
        let h = health(100_000_000, 50_000_000, 0);
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                assert!((dead_tup_ratio - (50.0 / 150.0)).abs() < 1e-9);
            }
            other => panic!("expected VacuumNeeded (precedence), got {other:?}"),
        }
    }

    #[test]
    fn recommend_partition_dominates_count_increase() {
        // An unpartitioned table cannot trigger CountIncrease at all
        // (CountIncrease requires `partition_count > 0`), so this test
        // pins the precedence boundary the other way: a partitioned
        // table that crosses both the size *and* the per-partition
        // ceiling falls into CountIncrease — there is no way for
        // PartitionRecommended to fire on a partitioned table by
        // construction.
        // Rule still under test: when both partition rules *could*
        // logically apply, PartitionRecommended only matches the
        // unpartitioned case (`partition_count == 0`).
        let h = health(100_000_000, 0, 4); // partitioned, 25M/partition
        match recommend(&h, 0.2, 10_000_000) {
            Recommendation::PartitionCountIncrease { current, suggested } => {
                assert_eq!(current, 4);
                assert_eq!(suggested, 8);
            }
            other => panic!("expected PartitionCountIncrease, got {other:?}"),
        }

        // And the inverse: unpartitioned + over threshold goes to
        // PartitionRecommended, NOT CountIncrease.
        let h = health(100_000_000, 0, 0);
        assert!(matches!(
            recommend(&h, 0.2, 10_000_000),
            Recommendation::PartitionRecommended { .. }
        ));
    }

    #[test]
    fn recommend_handles_n_tup_addition_overflow() {
        // Both counters at `i64::MAX` would panic in debug or silently
        // wrap in release under unchecked addition. `saturating_add`
        // caps at `i64::MAX`, so the ratio is `i64::MAX / i64::MAX = 1.0`
        // well above the default 0.2 threshold, so VacuumNeeded fires.
        // The test pins the contract: pathological stats values must NOT
        // panic and must still produce a deterministic recommendation.
        let h = TableHealth {
            table_name: "boom".to_string(),
            n_live_tup: i64::MAX,
            n_dead_tup: i64::MAX,
            last_analyze: None,
            partition_count: 0,
        };
        let result = recommend(&h, 0.2, 10_000_000);
        match result {
            Recommendation::VacuumNeeded { dead_tup_ratio } => {
                // i64::MAX / i64::MAX (saturated) = 1.0
                assert!(
                    (dead_tup_ratio - 1.0).abs() < 1e-9,
                    "expected ratio 1.0, got {dead_tup_ratio}"
                );
            }
            other => panic!("expected VacuumNeeded, got {other:?}"),
        }
    }

    /// Helper for the render-only tests: construct a small report with
    /// every recommendation arm represented. Sorted by table_name so
    /// the renderer's input mirrors what `run` would produce.
    fn fixture_report() -> Vec<(TableHealth, Recommendation)> {
        vec![
            (health(1_000, 0, 0), Recommendation::Healthy),
            (
                TableHealth {
                    table_name: "public.events".to_string(),
                    n_live_tup: 100_000_000,
                    n_dead_tup: 0,
                    last_analyze: None,
                    partition_count: 0,
                },
                Recommendation::PartitionRecommended {
                    reason:
                        "table has 100000000 live rows but is not partitioned (threshold: 10000000)"
                            .to_string(),
                },
            ),
            (
                TableHealth {
                    table_name: "public.orders".to_string(),
                    n_live_tup: 100_000_000,
                    n_dead_tup: 0,
                    last_analyze: None,
                    partition_count: 4,
                },
                Recommendation::PartitionCountIncrease {
                    current: 4,
                    suggested: 8,
                },
            ),
            (
                TableHealth {
                    table_name: "public.users".to_string(),
                    n_live_tup: 50,
                    n_dead_tup: 50,
                    last_analyze: None,
                    partition_count: 0,
                },
                Recommendation::VacuumNeeded {
                    dead_tup_ratio: 0.5,
                },
            ),
        ]
    }

    /// Render-only test: human format produces a sorted ASCII table
    /// whose body lines appear in input order. Pinning the format
    /// stops a future "improve the visuals" refactor from breaking
    /// downstream operator scripts that grep for substrings.
    #[test]
    fn render_human_lists_every_table_in_input_order() {
        let report = fixture_report();
        let mut buf = Vec::new();
        render_human(&report, &mut buf).expect("render");
        let s = String::from_utf8(buf).expect("utf8");

        // Header is present.
        assert!(s.contains("TABLE"), "missing header: {s}");
        assert!(s.contains("RECOMMENDATION"), "missing header: {s}");

        // Each fixture table appears exactly once in the body.
        for table in [
            "test_table",
            "public.events",
            "public.orders",
            "public.users",
        ] {
            assert_eq!(
                s.matches(table).count(),
                1,
                "table `{table}` should appear once: {s}"
            );
        }

        // Every recommendation tag shows up with its expected substring.
        assert!(s.contains("healthy"), "healthy tag missing: {s}");
        assert!(
            s.contains("vacuum (dead_tup_ratio="),
            "vacuum tag missing: {s}"
        );
        assert!(
            s.contains("partition (table has 100000000"),
            "partition tag missing: {s}"
        );
        assert!(
            s.contains("parts++ (current=4, suggested=8)"),
            "parts++ tag missing: {s}"
        );

        // Body line order matches input order.
        let test_idx = s.find("test_table").unwrap();
        let events_idx = s.find("public.events").unwrap();
        let orders_idx = s.find("public.orders").unwrap();
        let users_idx = s.find("public.users").unwrap();
        assert!(
            test_idx < events_idx && events_idx < orders_idx && orders_idx < users_idx,
            "input order not preserved: {s}"
        );
    }

    /// Regression sentinel — same input must produce byte-identical
    /// output across repeated invocations. Cements the determinism
    /// contract that `run` orchestrates: sort then render.
    #[test]
    fn render_human_is_byte_stable() {
        let report = fixture_report();
        let mut a = Vec::new();
        let mut b = Vec::new();
        render_human(&report, &mut a).unwrap();
        render_human(&report, &mut b).unwrap();
        assert_eq!(a, b, "render_human is not byte-stable");
    }

    /// Render-only test: JSON format parses through `serde_json::from_slice`
    /// as a non-empty array of well-formed objects, with every recommendation
    /// arm represented and the field order matching the projection struct.
    #[test]
    fn render_json_parses_and_field_order_is_stable() {
        let report = fixture_report();
        let mut buf = Vec::new();
        render_json(&report, &mut buf).expect("render");

        // 1. Parses through serde_json::Value (smoke test).
        let parsed: serde_json::Value =
            serde_json::from_slice(&buf).expect("output must be valid JSON");
        let array = parsed.as_array().expect("output must be a JSON array");
        assert_eq!(array.len(), 4, "expected 4 fixture rows: {parsed}");

        // 2. Every recommendation kind shows up at least once.
        let kinds: Vec<&str> = array
            .iter()
            .filter_map(|row| row.get("recommendation"))
            .filter_map(|rec| rec.get("kind"))
            .filter_map(|k| k.as_str())
            .collect();
        assert!(kinds.contains(&"healthy"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"vacuum_needed"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"partition_recommended"), "kinds: {kinds:?}");
        assert!(
            kinds.contains(&"partition_count_increase"),
            "kinds: {kinds:?}"
        );

        // 3. Field order on the first row matches the Row struct.
        // serde_json with `preserve_order` enabled (workspace dep
        // feature) preserves insertion order on Map deserialisation;
        // we walk the keys and assert the declared order.
        let first = array.first().expect("non-empty");
        let obj = first.as_object().expect("row must be an object");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "table_name",
                "n_live_tup",
                "n_dead_tup",
                "last_analyze",
                "partition_count",
                "recommendation",
            ],
            "field order must match Row struct declaration: {keys:?}"
        );

        // 4. Body is byte-stable across two renders.
        let mut buf2 = Vec::new();
        render_json(&report, &mut buf2).unwrap();
        assert_eq!(buf, buf2, "render_json is not byte-stable");
    }

    /// Empty input produces an empty JSON array — no crash, no
    /// "no rows" placeholder, just `[]\n` so downstream JSON parsers
    /// see a valid empty list.
    #[test]
    fn render_json_handles_empty_input() {
        let report: Vec<(TableHealth, Recommendation)> = Vec::new();
        let mut buf = Vec::new();
        render_json(&report, &mut buf).expect("render");
        let s = String::from_utf8(buf).expect("utf8");
        // `to_writer_pretty` emits `[]` for an empty Vec; the renderer
        // appends a newline.
        assert_eq!(s, "[]\n", "empty input should produce `[]\\n`, got: {s:?}");
    }

    /// Empty input produces just the header line in the human renderer.
    /// Pins the no-rows behaviour so a downstream `wc -l` sees the row
    /// count directly (header + 0 body lines = 1).
    #[test]
    fn render_human_handles_empty_input() {
        let report: Vec<(TableHealth, Recommendation)> = Vec::new();
        let mut buf = Vec::new();
        render_human(&report, &mut buf).expect("render");
        let s = String::from_utf8(buf).expect("utf8");
        // Exactly one line — the header.
        assert_eq!(
            s.lines().count(),
            1,
            "expected just a header line, got: {s:?}"
        );
        assert!(s.contains("TABLE"));
    }

    /// SQLSTATE classifier covers the three "pg_partman absent"
    /// codes and rejects anything else. Pure unit test — no DB.
    #[test]
    fn is_partman_absent_code_recognises_three_states() {
        assert!(is_partman_absent_code(&SqlState::UNDEFINED_FUNCTION));
        assert!(is_partman_absent_code(&SqlState::UNDEFINED_TABLE));
        assert!(is_partman_absent_code(&SqlState::INVALID_SCHEMA_NAME));
        // Non-absence codes must NOT count as partman-absent.
        assert!(!is_partman_absent_code(&SqlState::SYNTAX_ERROR));
        assert!(!is_partman_absent_code(&SqlState::INSUFFICIENT_PRIVILEGE));
        assert!(!is_partman_absent_code(&SqlState::CONNECTION_FAILURE));
    }

    /// `AnalyzeError::Display` must surface operator-actionable text
    /// for every variant — the CLI prints these straight to stderr.
    /// Every arm carries a remediation hint with a verb keyword
    /// (`Verify` / `Check` / `file an issue`). The keyword assertion is
    /// the regression sentinel: a future drift that drops the hint
    /// trips the test rather than silently regressing operator UX.
    /// All five variants are covered (Config, Pool, Db, Io, Json).
    #[test]
    fn analyze_error_display_is_operator_actionable() {
        // Config — surfaces the cause AND a remediation pointer to the
        // workspace + [database] section.
        let cfg = AnalyzeError::Config("bad toml".to_string());
        let s = format!("{cfg}");
        assert!(s.contains("config load"), "config display: {s}");
        assert!(s.contains("bad toml"), "config display: {s}");
        assert!(
            s.contains("Verify"),
            "config display must carry a remediation keyword: {s}"
        );
        assert!(
            s.contains("Djogi.toml"),
            "config display must point at Djogi.toml: {s}"
        );

        // Pool — surfaces the URL, the cause, AND a remediation pointer
        // to Djogi.toml::database.url.
        let pool = AnalyzeError::Pool {
            url: "postgres://localhost/x".to_string(),
            message: "refused".to_string(),
        };
        let s = format!("{pool}");
        assert!(s.contains("postgres://localhost/x"), "pool display: {s}");
        assert!(s.contains("refused"), "pool display: {s}");
        assert!(
            s.contains("Djogi.toml::database.url"),
            "pool display must point at config: {s}"
        );
        assert!(
            s.contains("Verify"),
            "pool display must carry a remediation keyword: {s}"
        );

        // Db — surfaces the cause AND a remediation pointer to
        // privileges + reachability.
        let db = AnalyzeError::Db("relation \"foo\" does not exist".to_string());
        let s = format!("{db}");
        assert!(s.contains("live-DB query"), "db display: {s}");
        assert!(s.contains("relation \"foo\""), "db display: {s}");
        assert!(
            s.contains("Verify"),
            "db display must carry a remediation keyword: {s}"
        );
        assert!(
            s.contains("pg_stat_user_tables"),
            "db display must mention the catalogue we query: {s}"
        );

        // Io — surfaces the cause AND a remediation pointer to
        // stdout/stderr permissions.
        let io = AnalyzeError::Io(std::io::Error::other("broken pipe"));
        let s = format!("{io}");
        assert!(s.contains("writing analyze output"), "io display: {s}");
        assert!(s.contains("broken pipe"), "io display: {s}");
        assert!(
            s.contains("Check"),
            "io display must carry a remediation keyword: {s}"
        );

        // Json — was previously omitted; covered now. Surfaces the cause
        // AND a "file an issue" hint because reaching this arm implies
        // an internal serde bug rather than operator misconfiguration.
        let json_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let json = AnalyzeError::Json(json_err);
        let s = format!("{json}");
        assert!(s.contains("encoding analyze output"), "json display: {s}");
        assert!(
            s.contains("file an issue"),
            "json display must point at the issue tracker: {s}"
        );
    }
}
