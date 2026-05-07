//! Phase 0 bootstrap — production-callable HeeRanjID + Postgres
//! extension installs.
//!
//! # Why this module exists
//!
//! `djogi migrations apply` and `djogi db reset` both need a virgin
//! Postgres database to be brought to a state where any descriptor-driven
//! migration can apply. That state is two pieces:
//!
//! 1. **HeeRanjID schema** — `heerid_next()` / `ranjid_next()` /
//!    the `heer_*` tables / the `current_heer_node_id` GUC reader.
//!    Every model that uses `HeerId` or `RanjId` as primary key
//!    references `DEFAULT heerid_next()` in its `CREATE TABLE` DDL,
//!    so the function must exist before the first descriptor-driven
//!    migration runs.
//!
//! 2. **Postgres extensions declared by descriptors** — `postgis`,
//!    `pgvector`, `pg_trgm`, etc. The differ tracks
//!    `IndexSchema::extension_dependency: Option<String>`; before a
//!    spatial / vector / trigram index can be created the matching
//!    extension must be installed.
//!
//! Pre-Track-0, only the test harness `setup_test_db_with_extensions`
//! installed these — the CLI / production / example paths hit a virgin
//! DB and failed on the very first migration that referenced
//! `DEFAULT heerid_next()`. The example papered over the gap with
//! hand-rolled `ctx.raw_ddl(...)` for HeeRanjID + PostGIS install.
//!
//! Track 0 lifts that bootstrap into this module:
//!
//! - SQL composition lives in `compose_*` functions that return owned
//!   `String`s. Pure, idempotent, deterministic — re-runs are no-ops.
//! - The runtime driver `run_phase_zero` executes the composed SQL
//!   via a `&tokio_postgres::GenericClient`. Used by both the test
//!   harness (sub-step 0.4) and the auto-emitted Phase 0 migration
//!   that `migrations compose` writes to disk (sub-step 0.3).
//!
//! # Idempotency
//!
//! Every install statement is idempotent in the sense that running
//! Phase 0 against an already-bootstrapped database is a clean no-op:
//!
//! - HeeRanjID's `INSTALL_SQL` uses `CREATE OR REPLACE FUNCTION` and
//!   `CREATE TABLE IF NOT EXISTS` throughout — re-running the install
//!   replaces function bodies and skips already-present tables.
//! - `CREATE EXTENSION IF NOT EXISTS` is a Postgres no-op when the
//!   extension is already installed.
//! - Seed inserts use `ON CONFLICT (...) DO NOTHING` — see
//!   `heeranjid::postgres_schema::SEED_SQL` for the exact column list.
//! - `ALTER DATABASE ... SET heer.node_id = '<n>'` is a metadata
//!   write that takes effect on every NEW connection — re-running it
//!   with the same value is a no-op.
//! - `SET heer.node_id = '<n>'` (session-level) is similarly a no-op
//!   when the value is unchanged.
//!
//! Re-runs are safe because `db reset` replays Phase 0 every cycle,
//! and the migration ledger replays Phase 0 once per fresh database.
//!
//! # No regex
//!
//! Per the project-wide no-regex rule, the extension-name validator
//! is implemented with byte-level checks (ASCII letter or underscore
//! followed by ASCII alphanumerics or underscores, up to 63 bytes).
//! See [`validate_extension_name`].
//!
//! # Public surface
//!
//! - [`PHASE_ZERO_VERSION`] — the canonical version label
//!   (`V00000000000000__phase_zero_bootstrap`) the auto-emit path
//!   stamps on the migration. Sorts lexically before any operator-
//!   composed migration (which use `V<YYYYMMDDHHMMSS>__<slug>` with
//!   year ≥ 1000), guaranteeing replay order.
//! - [`DEFAULT_NODE_ID`] — the default node id for single-node
//!   deployments, passed to [`run_phase_zero`] by most callers.
//! - [`run_phase_zero`] — runtime driver that installs HeeRanjID,
//!   required extensions, and the node-id GUC in one batch. The only
//!   entry point adopters and the test harness need.
//! - [`BootstrapError`] — error variants surfaced by the runtime
//!   driver.
//!
//! - [`ensure_phase_zero_emitted`] — idempotent per-database Phase 0
//!   emission called by `migrations compose`. Exposed as `pub` so the
//!   integration test suite can drive it directly.
//!
//! The SQL composition helpers (`compose_heeranjid_install`,
//! `compose_extension_installs`, `compose_node_seed`,
//! `compose_phase_zero`) are `pub(crate)` — used internally
//! by `migrations compose` and `db reset`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use tokio_postgres::GenericClient;

use super::compose::{AppLifecycle, PENDING_FORMAT_VERSION, PendingPlan};
use super::guard::WorkspaceGuard;
use super::ledger::compute_checksum;
use super::naming::{down_filename, up_filename};
use super::projection::BucketKey;
use super::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
use super::target::{bucket_dir, pending_database_dir, pending_json_path};

/// Default node id used by single-node deployments.
///
/// Matches the value `heeranjid::postgres_schema::seed_default_node`
/// inserts and the value the test harness hard-coded pre-Track-0.
/// Multi-node deployments override at the operator layer (separate
/// roadmap item — not in scope for Track 0).
pub const DEFAULT_NODE_ID: i32 = 1;

/// Canonical version label for the auto-emitted Phase 0 migration.
///
/// Uses an all-zero timestamp prefix so it sorts lexically before
/// every operator-composed migration (which use a real timestamp
/// `V<YYYYMMDDHHMMSS>__<slug>` with `YYYY >= 1000`). The runner +
/// `db reset` both replay migrations in lexical version order, so
/// Phase 0 always lands first on a fresh database.
///
/// The slug `phase_zero_bootstrap` is reserved — operators cannot
/// compose a migration with this slug and an all-zero timestamp
/// because the version-prefix grammar requires `version_prefix(now)`
/// which always reflects a wall-clock instant.
pub const PHASE_ZERO_VERSION: &str = "V00000000000000__phase_zero_bootstrap";

/// Sorted allowlist of Postgres extensions Djogi knows how to install.
///
/// Validated via `binary_search` in [`compose_extension_installs`] —
/// descriptor-derived extension names that are not in this list are
/// rejected before any SQL is emitted.  Adding a new extension to
/// Djogi's descriptor system requires a matching entry here.
///
/// Rules: ASCII lowercase, sorted lexically (binary_search requirement).
pub(crate) const ALLOWED_EXTENSIONS: &[&str] = &[
    "btree_gist",
    "pg_trgm",
    "pgcrypto",
    "pgvector",
    "postgis",
    "vector",
];

/// Errors surfaced by the bootstrap entry points.
#[derive(Debug)]
pub enum BootstrapError {
    /// Caller supplied an extension name that does not match the
    /// Postgres-identifier grammar (ASCII letter or underscore
    /// followed by ASCII alphanumerics or underscores, 1–63 bytes).
    InvalidExtensionName {
        /// The offending name, preserved for diagnostics.
        name: String,
    },
    /// Caller supplied a valid identifier that is not in Djogi's
    /// known-extension allowlist ([`ALLOWED_EXTENSIONS`]).  Adding new
    /// Postgres extensions to Djogi requires updating that list.
    UnknownExtension {
        /// The offending name, preserved for diagnostics.
        name: String,
    },
    /// Underlying `tokio_postgres` error from one of the install
    /// batches. The wrapping kept generic so callers can match on the
    /// step (`heeranjid_install`, `extension_install`, `node_seed`)
    /// to surface a more specific operator message.
    Db {
        /// Operator-facing label for the failing install step.
        step: &'static str,
        /// Source error from `tokio_postgres`.
        source: tokio_postgres::Error,
    },
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::InvalidExtensionName { name } => write!(
                f,
                "phase 0 bootstrap: extension name `{name}` does not match the \
                 Postgres-identifier grammar (ASCII letter or underscore followed \
                 by ASCII alphanumerics or underscores, 1-63 bytes)"
            ),
            BootstrapError::UnknownExtension { name } => write!(
                f,
                "phase 0 bootstrap: extension `{name}` is not in Djogi's \
                 known-extension allowlist; add it to ALLOWED_EXTENSIONS in \
                 migrate/bootstrap.rs to enable installation"
            ),
            BootstrapError::Db { step, source } => {
                write!(f, "phase 0 bootstrap: {step} failed: {source}")
            }
        }
    }
}

impl std::error::Error for BootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BootstrapError::Db { source, .. } => Some(source),
            BootstrapError::InvalidExtensionName { .. }
            | BootstrapError::UnknownExtension { .. } => None,
        }
    }
}

// ── SQL composition (pure) ────────────────────────────────────────────────

/// Compose the HeeRanjID install SQL.
///
/// Includes the base `INSTALL_SQL` (schema + session helpers +
/// `generate_id` + `generate_ranj_id`), the v0.3 desc-support
/// primitives (`heerid_to_desc`, `*_next_desc`, bulk backfill), and
/// the default-node seed. All of these are idempotent — re-running
/// against an already-installed database is a no-op.
///
/// Returns an owned `String` so the caller can hash it into the
/// migration's checksum, write it to disk verbatim, or feed it to
/// `client.batch_execute` directly.
///
/// **Why this function** rather than calling
/// `heeranjid::postgres_schema::install_schema` etc. directly: the
/// auto-emit path needs the SQL as a `String` to write into the
/// `<workspace>/migrations/<db>/<app>/V00000000000000__phase_zero_bootstrap.sql`
/// file. The runtime test-harness path also benefits — a single
/// composed blob means one `batch_execute` call with one round-trip,
/// instead of four.
pub(crate) fn compose_heeranjid_install() -> String {
    // The order here mirrors what the test harness ran pre-Track-0:
    // base install, desc-support primitives, seed.
    //
    // Each blob from heeranjid is already a self-contained CREATE
    // OR REPLACE / CREATE IF NOT EXISTS / ON CONFLICT DO NOTHING
    // sequence. We concatenate with explicit blank lines + section
    // comments so the on-disk migration file is readable.
    let mut out = String::with_capacity(
        heeranjid::postgres_schema::INSTALL_SQL.len()
            + heeranjid::postgres_schema::DESC_FLIP_SQL.len()
            + heeranjid::postgres_schema::DESC_GENERATORS_SQL.len()
            + heeranjid::postgres_schema::BULK_BACKFILL_SQL.len()
            + heeranjid::postgres_schema::SEED_SQL.len()
            + 512,
    );
    out.push_str("-- HeeRanjID base schema + functions (idempotent).\n");
    out.push_str(heeranjid::postgres_schema::INSTALL_SQL);
    out.push_str("\n\n-- HeeRanjID desc-flip primitives (heerid_to_desc / ranjid_to_desc / heerid_flip_mask).\n");
    out.push_str(heeranjid::postgres_schema::DESC_FLIP_SQL);
    out.push_str("\n\n-- HeeRanjID single-row generators + *_next_desc generators.\n");
    out.push_str(heeranjid::postgres_schema::DESC_GENERATORS_SQL);
    out.push_str("\n\n-- HeeRanjID migration-support procedures (bulk backfill).\n");
    out.push_str(heeranjid::postgres_schema::BULK_BACKFILL_SQL);
    out.push_str("\n\n-- HeeRanjID default-node seed (node_id = 1, ON CONFLICT DO NOTHING).\n");
    out.push_str(heeranjid::postgres_schema::SEED_SQL);
    out
}

/// Compose `CREATE EXTENSION IF NOT EXISTS "<name>"` statements for
/// each entry in the supplied set, one per line, in sorted order.
///
/// Names are validated against the Postgres-identifier grammar before
/// any output is emitted; an invalid name surfaces as
/// [`BootstrapError::InvalidExtensionName`] with the offending
/// value preserved for the operator message. Validation runs first
/// so the caller can fail fast — partial output is never produced.
///
/// `IF NOT EXISTS` makes the statement idempotent; re-running against
/// an already-installed extension is a no-op. Names are double-quoted
/// so the SQL is safe even if a future extension name were to collide
/// with a Postgres keyword.
///
/// An empty set returns an empty string. Callers concatenate this
/// into [`compose_phase_zero`] without conditional handling.
pub(crate) fn compose_extension_installs(
    extensions: &BTreeSet<String>,
) -> Result<String, BootstrapError> {
    // Two-pass validation: identifier grammar first (cheap), then
    // allowlist (binary_search on ALLOWED_EXTENSIONS).  Both passes
    // run before any output String is allocated so callers either get
    // clean SQL or a structured error — never partial output.
    for name in extensions {
        validate_extension_name(name)?;
        if ALLOWED_EXTENSIONS.binary_search(&name.as_str()).is_err() {
            return Err(BootstrapError::UnknownExtension { name: name.clone() });
        }
    }
    if extensions.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::with_capacity(extensions.iter().map(|s| s.len() + 32).sum());
    out.push_str("-- Postgres extensions required by descriptor inventory (idempotent).\n");
    for name in extensions {
        // Double-quote defensively even though `validate_extension_name`
        // already restricts the byte set — keeps the emitted SQL
        // resilient against future spec changes.
        out.push_str("CREATE EXTENSION IF NOT EXISTS \"");
        out.push_str(name);
        out.push_str("\";\n");
    }
    Ok(out)
}

/// Compose the node-id seed SQL — the `ALTER DATABASE` (database-level
/// GUC for new connections) and a session-level `SET` (so the running
/// connection that just executed Phase 0 sees the value immediately,
/// without needing to drop and re-establish).
///
/// Both statements are idempotent: re-running with the same value is
/// a metadata-only no-op on the database side and a session-write
/// no-op on the client side.
///
/// **Why both** an ALTER DATABASE and a session SET: the Phase 0 SQL
/// runs through whatever connection the runner has — typically a
/// pool-backed `tokio_postgres::Client`. The pool's `post_connect`
/// hook (set in `pg::pool`) sets `heer.node_id` per-connection for
/// every NEW connection it opens. The ALTER DATABASE persists the
/// default so freshly-opened connections inherit it without needing
/// the post-connect hook (belt-and-braces). The session-level SET
/// covers the running connection itself — without it, an additive
/// migration applied immediately after Phase 0 in the same `apply`
/// run would lack the GUC and `current_heer_node_id()` would surface
/// a `nan` / null read.
///
/// `node_id` must be a non-negative `i32`; the SQL uses the raw
/// integer (no quoting) which is safe because the type is integer-
/// only.
///
/// **Why an unquoted database name** is acceptable here: the
/// production caller passes the database name from
/// `extract_database_from_url`, which round-trips through
/// `is_valid_pg_identifier` (ASCII letter or underscore followed by
/// ASCII alphanumerics or underscores, 1-63 bytes). The bootstrap
/// composer re-validates as defence-in-depth so a future caller that
/// skips the URL helper still gets a typed error rather than an SQL
/// injection.
pub(crate) fn compose_node_seed(database: &str, node_id: i32) -> Result<String, BootstrapError> {
    // Re-validate the database identifier even though production
    // callers pre-validate via `extract_database_from_url` +
    // `is_valid_pg_identifier`. Defence-in-depth — a mis-routed
    // caller still gets a typed error rather than an SQL injection.
    validate_extension_name(database)?;
    let mut out = String::with_capacity(database.len() + 96);
    out.push_str("-- HeeRanjID node-id GUC seed (database-level + session-level).\n");
    out.push_str("ALTER DATABASE \"");
    out.push_str(database);
    out.push_str("\" SET heer.node_id = '");
    out.push_str(&node_id.to_string());
    out.push_str("';\n");
    out.push_str("SET heer.node_id = '");
    out.push_str(&node_id.to_string());
    out.push_str("';\n");
    Ok(out)
}

/// Compose the complete Phase 0 SQL — HeeRanjID install + extensions
/// + node seed, in dependency order.
///
/// Consumers:
/// - `migrations compose` writes this to
///   `<workspace>/migrations/<db>/<app>/V00000000000000__phase_zero_bootstrap.sql`
///   and tracks it in the ledger like any other migration.
/// - The test harness `setup_test_db_with_extensions` runs this
///   directly via [`run_phase_zero`] before applying pending
///   migrations.
///
/// Order matters: HeeRanjID schema must exist before any extension
/// install runs (in case an extension's setup script touches the
/// `heer` schema), and both must exist before the node seed runs (in
/// case the seed relies on extension-provided types).
///
/// Returns owned bytes so the caller can hash, write, or execute
/// directly.
pub(crate) fn compose_phase_zero(
    database: &str,
    extensions: &BTreeSet<String>,
    node_id: i32,
) -> Result<String, BootstrapError> {
    let heeranjid = compose_heeranjid_install();
    let exts = compose_extension_installs(extensions)?;
    let node = compose_node_seed(database, node_id)?;
    let mut out = String::with_capacity(heeranjid.len() + exts.len() + node.len() + 256);
    out.push_str("-- ╭───────────────────────────────────────────────────────────────╮\n");
    out.push_str("-- │ Djogi Phase 0 bootstrap — HeeRanjID + extensions + node seed │\n");
    out.push_str("-- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n");
    out.push_str("-- ╰───────────────────────────────────────────────────────────────╯\n\n");
    out.push_str(&heeranjid);
    if !exts.is_empty() {
        out.push_str("\n\n");
        out.push_str(&exts);
    }
    out.push_str("\n\n");
    out.push_str(&node);
    Ok(out)
}

// ── Runtime driver ────────────────────────────────────────────────────────

/// Execute Phase 0 SQL against a live Postgres connection.
///
/// Used by the test harness (sub-step 0.4) and any production caller
/// that wants to bring a fresh database to a Phase-0 state without
/// going through the on-disk migration ledger (e.g. one-shot
/// provisioning scripts).
///
/// **Single batch.** The composed SQL is sent through `batch_execute`
/// in one round-trip. Postgres parses + executes the whole batch
/// inside an implicit transaction (no `BEGIN`/`COMMIT` in the SQL),
/// so a partial failure rolls back cleanly and leaves the database
/// at its prior state.
///
/// **Idempotent.** See module-level idempotency notes — every
/// statement uses `CREATE OR REPLACE`, `IF NOT EXISTS`, or
/// `ON CONFLICT DO NOTHING` so re-running against an already-
/// bootstrapped database is a no-op.
///
/// `node_id` defaults to [`DEFAULT_NODE_ID`] (1) for single-node
/// deployments. Multi-node operators pass their own value.
///
/// # Errors
///
/// - [`BootstrapError::InvalidExtensionName`] when an extension name
///   does not match the Postgres-identifier grammar — surfaced
///   BEFORE any SQL runs so partial-state is impossible.
/// - [`BootstrapError::Db`] when `batch_execute` fails. The
///   `step: "phase_zero"` discriminator distinguishes this from
///   future per-step error classes if the function is split later.
#[allow(clippy::disallowed_methods)]
pub async fn run_phase_zero<C>(
    client: &C,
    database: &str,
    extensions: &BTreeSet<String>,
    node_id: i32,
) -> Result<(), BootstrapError>
where
    C: GenericClient + ?Sized,
{
    let sql = compose_phase_zero(database, extensions, node_id)?;
    client
        .batch_execute(&sql)
        .await
        .map_err(|source| BootstrapError::Db {
            step: "phase_zero",
            source,
        })?;
    Ok(())
}

// ── Descriptor inspection helper ──────────────────────────────────────────

/// Collect the distinct, sorted set of `extension_dependency` values
/// across every index in every model in the per-bucket `AppliedSchema`
/// map (all databases combined).
///
/// Production compose uses [`extensions_for_database`] to aggregate
/// per-database.  This cross-database variant exists for unit tests
/// that verify deduplication semantics across multiple buckets.
#[cfg(test)]
fn extension_dependencies_from_models(
    models: &BTreeMap<BucketKey, AppliedSchema>,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for schema in models.values() {
        for index in &schema.indexes {
            if let Some(dep) = &index.extension_dependency {
                deps.insert(dep.clone());
            }
        }
    }
    deps
}

// ── Auto-emit (sub-step 0.3) ──────────────────────────────────────────────

/// One database that received a Phase 0 emission during this compose
/// run. Returned in [`EmittedPhaseZero`] reports so the CLI can log
/// structured progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedPhaseZero {
    /// Database name. Phase 0 is emitted into the synthetic
    /// `_global_` bucket of this database.
    pub database: String,
    /// Final on-disk path of the up-side migration file.
    pub up_sql_path: PathBuf,
    /// Final on-disk path of the down-side migration file.
    pub down_sql_path: PathBuf,
    /// Final on-disk path of the pending JSON file.
    pub pending_json_path: PathBuf,
    /// Distinct extensions baked into this Phase 0 emission. Useful
    /// for logging — adopters often want to see which Postgres
    /// extensions were detected.
    pub extensions: BTreeSet<String>,
}

/// Errors surfaced by [`ensure_phase_zero_emitted`].
///
/// Distinct from [`BootstrapError`] because the auto-emit path also
/// touches the filesystem — I/O errors and bootstrap composition
/// errors are different kinds of failure and the caller (compose)
/// surfaces them through different `ComposeError` variants.
#[derive(Debug)]
pub enum AutoEmitError {
    /// Bootstrap composition failed (e.g. invalid extension name).
    Compose(BootstrapError),
    /// Filesystem I/O failed at the named path.
    Io { path: PathBuf, source: io::Error },
    /// Pending JSON serialization failed.
    PendingJson(serde_json::Error),
}

impl std::fmt::Display for AutoEmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutoEmitError::Compose(e) => write!(f, "phase 0 auto-emit: {e}"),
            AutoEmitError::Io { path, source } => write!(
                f,
                "phase 0 auto-emit: i/o failure at {}: {source}",
                path.display()
            ),
            AutoEmitError::PendingJson(e) => {
                write!(f, "phase 0 auto-emit: pending JSON serialization: {e}")
            }
        }
    }
}

impl std::error::Error for AutoEmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AutoEmitError::Compose(e) => Some(e),
            AutoEmitError::Io { source, .. } => Some(source),
            AutoEmitError::PendingJson(e) => Some(e),
        }
    }
}

impl From<BootstrapError> for AutoEmitError {
    fn from(e: BootstrapError) -> Self {
        AutoEmitError::Compose(e)
    }
}

/// Walk every database referenced in `models` ∪ `apps` and emit a
/// Phase 0 bootstrap migration for any database that doesn't already
/// have one on disk.
///
/// **When this fires.** A database is considered "missing Phase 0"
/// when the file
/// `<workspace>/migrations/<database>/_global_/V00000000000000__phase_zero_bootstrap.sql`
/// does not exist. Once it exists, subsequent compose runs leave it
/// untouched (idempotent — running compose twice never re-emits).
///
/// **What gets baked in.** The Phase 0 SQL composition (per
/// [`compose_phase_zero`]) snapshots the descriptor inventory's
/// extension dependencies AT THE TIME of the first compose. Future
/// compose runs that introduce NEW extensions (e.g. adding a
/// `pgvector` index after PostGIS was already in use) emit a regular
/// migration via the standard delta path — they do NOT re-emit
/// Phase 0. The trade-off: simpler invariant (Phase 0 is one-shot)
/// at the cost of a separate `CREATE EXTENSION` migration for any
/// extension added later.
///
/// **Bucket placement.** Phase 0 lives in the synthetic global
/// bucket (empty-string app label) of each database, on disk at
/// `migrations/<database>/_global_/`. This means the auto-emit fires
/// once per `(database, "")` pair and the regular `_global_` app
/// space carries one extra "fixed point" migration that always
/// applies first.
///
/// **Down side.** Phase 0 is bootstrap — there is no meaningful
/// rollback. The down-side file is comment-only with an explicit
/// "phase 0 has no rollback" marker. `db reset` never invokes the
/// down side; this exists only to satisfy the migration-pair
/// convention and keep tooling that reads the down side simple.
///
/// **Pending JSON.** The pending JSON tracks Phase 0 the same way as
/// any composed migration. Its `model_snapshot` is the empty schema
/// because Phase 0 doesn't define any user-visible schema — it only
/// installs framework dependencies.
///
/// **Idempotency contract.**
///
/// - Running compose against a workspace that already has Phase 0
///   on disk is a no-op for the Phase 0 path. Returns
///   `Ok(Vec::new())`.
/// - Running compose against a workspace with NO Phase 0 yet emits
///   exactly one Phase 0 per database in the inputs.
///
/// **Witness-typed lock.** The `_guard: &WorkspaceGuard` parameter
/// is a compile-time witness that the workspace lock is held — the
/// same convention compose / runner use. The function does not
/// touch the guard; the parameter is named with a leading underscore
/// to signal "consumed at the type level only".
pub fn ensure_phase_zero_emitted(
    workspace_root: &Path,
    models: &BTreeMap<BucketKey, AppliedSchema>,
    apps: &[AppLifecycle],
    now: OffsetDateTime,
    _guard: &WorkspaceGuard,
) -> Result<Vec<EmittedPhaseZero>, AutoEmitError> {
    // 1. Collect the distinct database set from inputs.
    //
    //    Sources:
    //      - Every bucket in `models` carries a `database`.
    //      - Every entry in `apps` carries a `database` — covers the
    //        case where an app is registered but has no models yet
    //        (still needs its database bootstrapped).
    let mut databases: BTreeSet<String> = BTreeSet::new();
    for bucket in models.keys() {
        databases.insert(bucket.database.clone());
    }
    for app in apps {
        databases.insert(app.database.clone());
    }

    // 2. For each database, decide whether to emit. Skip databases
    //    that already have Phase 0 on disk.
    let mut emitted: Vec<EmittedPhaseZero> = Vec::new();
    for database in &databases {
        let bucket = BucketKey {
            database: database.clone(),
            app: String::new(),
        };
        let dir = bucket_dir(workspace_root, &bucket);
        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let down_path = dir.join(down_filename(PHASE_ZERO_VERSION));
        let pending_path = pending_json_path(workspace_root, &bucket);

        // All three artifacts must be present for the emit to be
        // considered complete. Checking only `up_path` would skip
        // re-emission on a partial write (e.g. crash after up.sql but
        // before pending.json), leaving a stale disk state forever.
        if up_path.exists() && down_path.exists() && pending_path.exists() {
            continue;
        }

        // 3. Compose the Phase 0 SQL for this database.
        //
        //    Aggregate extensions from EVERY bucket in this database
        //    (per-bucket `app` is irrelevant — extensions are a
        //    Postgres-cluster-level concept and PostGIS-on-billing is
        //    the same install as PostGIS-on-shipping).
        let extensions = extensions_for_database(models, database);
        let up_sql = compose_phase_zero(database, &extensions, DEFAULT_NODE_ID)?;
        let down_sql = compose_phase_zero_down_text();

        // 4. Build the pending JSON. `model_snapshot` is empty —
        //    Phase 0 doesn't capture any user schema, only framework
        //    bootstrap. Production callers re-derive snapshots
        //    on subsequent composes; the Phase 0 row in the ledger
        //    survives the snapshot rebuild.
        let pending = PendingPlan {
            format_version: PENDING_FORMAT_VERSION.to_string(),
            bucket_database: database.clone(),
            bucket_app: String::new(),
            version: PHASE_ZERO_VERSION.to_string(),
            slug: PHASE_ZERO_SLUG.to_string(),
            model_snapshot: empty_schema_for(&bucket),
            checksum_up: compute_checksum([up_sql.as_str()]),
            checksum_down: None, // comment-only, no real rollback
            composed_at: format_rfc3339_seconds(now),
        };
        let pending_bytes =
            serde_json::to_vec_pretty(&pending).map_err(AutoEmitError::PendingJson)?;

        // 5. Ensure parent directories exist for the bucket dir and
        //    the pending dir, then write all three files.
        ensure_parent(&up_path)?;
        ensure_parent(&pending_path)?;
        fs::create_dir_all(&dir).map_err(|e| AutoEmitError::Io {
            path: dir.clone(),
            source: e,
        })?;
        fs::create_dir_all(pending_database_dir(workspace_root, database)).map_err(|e| {
            AutoEmitError::Io {
                path: pending_database_dir(workspace_root, database),
                source: e,
            }
        })?;
        fs::write(&up_path, up_sql.as_bytes()).map_err(|e| AutoEmitError::Io {
            path: up_path.clone(),
            source: e,
        })?;
        fs::write(&down_path, down_sql.as_bytes()).map_err(|e| AutoEmitError::Io {
            path: down_path.clone(),
            source: e,
        })?;
        fs::write(&pending_path, &pending_bytes).map_err(|e| AutoEmitError::Io {
            path: pending_path.clone(),
            source: e,
        })?;

        emitted.push(EmittedPhaseZero {
            database: database.clone(),
            up_sql_path: up_path,
            down_sql_path: down_path,
            pending_json_path: pending_path,
            extensions,
        });
    }
    Ok(emitted)
}

/// Slug component of [`PHASE_ZERO_VERSION`]. Exposed so consumers
/// that need to construct the version string from parts (rare) can
/// stay aligned with the canonical label.
const PHASE_ZERO_SLUG: &str = "phase_zero_bootstrap";

/// Compose the Phase 0 down-side SQL — comment-only.
///
/// Phase 0 has no meaningful rollback: dropping the HeeRanjID schema
/// would invalidate every model's primary key, dropping PostGIS
/// would invalidate every spatial column. `db reset` re-replays
/// Phase 0 from scratch on the recreated database; rollback through
/// the down side is not a supported flow.
///
/// We emit a comment block rather than an empty file so tooling that
/// reads the down side (status reports, diff views) sees an explicit
/// "no rollback" message rather than a silent empty file that might
/// be mistaken for missing or corrupt.
fn compose_phase_zero_down_text() -> String {
    let mut out = String::with_capacity(512);
    out.push_str("-- Djogi Phase 0 bootstrap — down (no-op).\n");
    out.push_str("--\n");
    out.push_str("-- Phase 0 installs framework dependencies (HeeRanjID schema +\n");
    out.push_str("-- Postgres extensions + node-id GUC) that every subsequent\n");
    out.push_str("-- migration depends on. Rolling those back would invalidate the\n");
    out.push_str("-- entire schema, so Phase 0 has no meaningful down side.\n");
    out.push_str("--\n");
    out.push_str("-- `djogi db reset` re-replays Phase 0 from scratch on the\n");
    out.push_str("-- recreated database. The migration ledger tracks Phase 0 like\n");
    out.push_str("-- any other migration; rolling it back is not a supported flow.\n");
    out
}

/// Aggregate extension dependencies from every bucket in a single
/// database.
///
/// PostGIS declared in `main.billing` and `main.shipping` merges to
/// one install in `main`'s Phase 0.  PostGIS declared in
/// `crud_log.audit` lives in a separate Phase 0 for the `crud_log`
/// database.
fn extensions_for_database(
    models: &BTreeMap<BucketKey, AppliedSchema>,
    database: &str,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for (bucket, schema) in models {
        if bucket.database != database {
            continue;
        }
        for index in &schema.indexes {
            if let Some(dep) = &index.extension_dependency {
                deps.insert(dep.clone());
            }
        }
    }
    deps
}

/// Ensure the parent directory of `path` exists; create it (and any
/// missing intermediates) when absent. Mirrors the helper used in
/// `compose.rs` — duplicated here so `bootstrap` does not import
/// internals from a peer module.
fn ensure_parent(path: &Path) -> Result<(), AutoEmitError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| AutoEmitError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

/// Build an empty [`AppliedSchema`] for the supplied bucket — used as
/// the Phase 0 pending JSON's `model_snapshot` since Phase 0 captures
/// no user schema.
fn empty_schema_for(bucket: &BucketKey) -> AppliedSchema {
    AppliedSchema {
        djogi_version: env!("CARGO_PKG_VERSION").to_string(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: format_rfc3339_seconds(OffsetDateTime::now_utc()),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: vec![bucket.app.clone()],
    }
}

/// Format an instant as `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, UTC,
/// second precision). Mirrors [`super::projection::rfc3339_now_seconds`]
/// so the Phase 0 pending JSON's `composed_at` field matches the
/// shape every other compose-emitted pending file uses.
fn format_rfc3339_seconds(instant: OffsetDateTime) -> String {
    let utc = instant.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        utc.year(),
        utc.month() as u8,
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second()
    )
}

// ── Validators ────────────────────────────────────────────────────────────

/// Validate that `name` is a plain Postgres identifier safe to
/// interpolate into a `CREATE EXTENSION IF NOT EXISTS "<name>"`
/// statement (or to splice into `ALTER DATABASE "<name>"`).
///
/// Rules (byte-level, no regex per the Djogi-wide no-regex policy):
///
/// - Length between 1 and 63 bytes inclusive (Postgres `NAMEDATALEN`
///   minus the trailing `NUL`).
/// - First byte is an ASCII letter (upper- or lower-case) or
///   underscore.
/// - Every subsequent byte is an ASCII letter, digit, or underscore.
///
/// All real-world extension names on PGXN and in `contrib/`
/// (`postgis`, `pg_trgm`, `pgcrypto`, `pgvector`, `vector`) match
/// this rule.
fn validate_extension_name(name: &str) -> Result<(), BootstrapError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return Err(BootstrapError::InvalidExtensionName {
            name: name.to_string(),
        });
    }
    let first = bytes[0];
    let first_ok = first.is_ascii_alphabetic() || first == b'_';
    if !first_ok {
        return Err(BootstrapError::InvalidExtensionName {
            name: name.to_string(),
        });
    }
    for &b in &bytes[1..] {
        let ok = b.is_ascii_alphanumeric() || b == b'_';
        if !ok {
            return Err(BootstrapError::InvalidExtensionName {
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_extension_name_accepts_real_names() {
        assert!(validate_extension_name("postgis").is_ok());
        assert!(validate_extension_name("pg_trgm").is_ok());
        assert!(validate_extension_name("pgcrypto").is_ok());
        assert!(validate_extension_name("pgvector").is_ok());
        assert!(validate_extension_name("vector").is_ok());
        assert!(validate_extension_name("_internal").is_ok());
        // Boundary: exactly 63 bytes.
        assert!(validate_extension_name(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn validate_extension_name_rejects_bad_inputs() {
        assert!(validate_extension_name("").is_err());
        assert!(validate_extension_name(&"a".repeat(64)).is_err());
        assert!(validate_extension_name("1starts_with_digit").is_err());
        assert!(validate_extension_name("has-dash").is_err());
        assert!(validate_extension_name("has space").is_err());
        assert!(validate_extension_name("has\"quote").is_err());
        assert!(validate_extension_name("has;semicolon").is_err());
        assert!(validate_extension_name("naïve").is_err()); // multibyte UTF-8
    }

    #[test]
    fn compose_extension_installs_empty_set_returns_empty_string() {
        let empty = BTreeSet::new();
        assert_eq!(compose_extension_installs(&empty).unwrap(), "");
    }

    #[test]
    fn compose_extension_installs_emits_sorted_idempotent_statements() {
        let mut s = BTreeSet::new();
        s.insert("postgis".to_string());
        s.insert("pg_trgm".to_string());
        s.insert("pgvector".to_string());
        let sql = compose_extension_installs(&s).unwrap();
        // BTreeSet iteration is sorted.
        let pg_trgm = sql.find("\"pg_trgm\"").expect("pg_trgm present");
        let pgvector = sql.find("\"pgvector\"").expect("pgvector present");
        let postgis = sql.find("\"postgis\"").expect("postgis present");
        assert!(pg_trgm < pgvector, "pg_trgm should sort before pgvector");
        assert!(pgvector < postgis, "pgvector should sort before postgis");
        // Idempotent.
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS"));
    }

    #[test]
    fn compose_extension_installs_rejects_bad_name() {
        let mut s = BTreeSet::new();
        s.insert("good_name".to_string());
        s.insert("bad name".to_string());
        match compose_extension_installs(&s) {
            Err(BootstrapError::InvalidExtensionName { name }) => {
                assert_eq!(name, "bad name");
            }
            other => panic!("expected InvalidExtensionName, got {other:?}"),
        }
    }

    #[test]
    fn compose_node_seed_emits_alter_and_session_set() {
        let sql = compose_node_seed("djogi_test_abc", 7).unwrap();
        assert!(sql.contains("ALTER DATABASE \"djogi_test_abc\" SET heer.node_id = '7'"));
        assert!(sql.contains("SET heer.node_id = '7'"));
    }

    #[test]
    fn compose_node_seed_rejects_bad_database_name() {
        match compose_node_seed("bad name", 1) {
            Err(BootstrapError::InvalidExtensionName { name }) => {
                assert_eq!(name, "bad name");
            }
            other => panic!("expected InvalidExtensionName, got {other:?}"),
        }
    }

    #[test]
    fn compose_heeranjid_install_includes_all_blobs() {
        let sql = compose_heeranjid_install();
        // Sanity: every section header is present.
        assert!(sql.contains("HeeRanjID base schema"));
        assert!(sql.contains("desc-flip primitives"));
        assert!(sql.contains("single-row generators"));
        assert!(sql.contains("bulk backfill"));
        assert!(sql.contains("default-node seed"));
        // Sanity: at least one heeranjid SQL token survived passthrough.
        assert!(sql.contains("heer_nodes") || sql.contains("generate_id"));
    }

    #[test]
    fn compose_phase_zero_orders_install_then_extensions_then_seed() {
        let mut exts = BTreeSet::new();
        exts.insert("postgis".to_string());
        let sql = compose_phase_zero("djogi_test_db", &exts, 1).unwrap();
        let install_idx = sql.find("HeeRanjID base schema").expect("install present");
        let ext_idx = sql.find("CREATE EXTENSION").expect("extension present");
        let seed_idx = sql.find("ALTER DATABASE").expect("seed present");
        assert!(install_idx < ext_idx, "install must precede extensions");
        assert!(ext_idx < seed_idx, "extensions must precede node seed");
    }

    #[test]
    fn compose_phase_zero_omits_extension_section_when_empty() {
        let exts = BTreeSet::new();
        let sql = compose_phase_zero("djogi_test_db", &exts, 1).unwrap();
        assert!(!sql.contains("CREATE EXTENSION"));
        assert!(sql.contains("ALTER DATABASE"));
    }

    #[test]
    fn extension_dependencies_from_models_dedups_across_buckets() {
        use crate::migrate::schema::{
            IndexKindSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
        };

        let mk_index = |name: &str, dep: Option<&str>| IndexSchema {
            extension_dependency: dep.map(|s| s.to_string()),
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::NonUnique,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: "t".to_string(),
            target: IndexTargetSchema::Columns(Vec::new()),
        };
        let mk_schema = |indexes: Vec<IndexSchema>| AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: super::super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-05-04T00:00:00Z".to_string(),
            indexes,
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        };

        let mut models = BTreeMap::new();
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            mk_schema(vec![
                mk_index("idx_a", Some("postgis")),
                mk_index("idx_b", Some("pg_trgm")),
            ]),
        );
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "shipping".to_string(),
            },
            mk_schema(vec![
                mk_index("idx_c", Some("postgis")), // duplicate across buckets
                mk_index("idx_d", None),            // no dependency — skipped
                mk_index("idx_e", Some("pgvector")),
            ]),
        );

        let deps = extension_dependencies_from_models(&models);
        let expected: BTreeSet<String> = ["pg_trgm", "pgvector", "postgis"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(deps, expected);
    }

    // ── ensure_phase_zero_emitted (auto-emit, sub-step 0.3) tests ─────────

    use crate::migrate::guard::WorkspaceGuard;
    use crate::migrate::guard::acquire as acquire_workspace_lock;
    use std::time::Duration;

    /// Per-test workspace root + lock guard. Each test gets its own
    /// unique paths so concurrent runs do not collide.
    fn temp_workspace_with_guard(label: &str) -> (PathBuf, WorkspaceGuard) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("djogi-bootstrap-test-{label}-{stamp}"));
        std::fs::create_dir_all(&root).expect("create workspace root");
        let lock_path = root.join(crate::migrate::guard::LOCK_FILE_NAME);
        let guard = acquire_workspace_lock(&lock_path, Duration::from_secs(5))
            .expect("acquire workspace lock");
        (root, guard)
    }

    fn fixed_now() -> OffsetDateTime {
        let date = time::Date::from_calendar_date(2026, time::Month::May, 4).unwrap();
        let t = time::Time::from_hms(12, 0, 0).unwrap();
        date.with_time(t).assume_utc()
    }

    #[test]
    fn ensure_phase_zero_emits_for_empty_workspace_with_apps() {
        let (work, guard) = temp_workspace_with_guard("auto_empty_apps");
        let apps = vec![AppLifecycle {
            label: String::new(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];
        let models = BTreeMap::new();
        let emitted =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("emit");
        assert_eq!(emitted.len(), 1, "one Phase 0 per database");
        assert_eq!(emitted[0].database, "main");
        assert!(emitted[0].extensions.is_empty(), "no extensions in models");
        assert!(emitted[0].up_sql_path.exists());
        assert!(emitted[0].down_sql_path.exists());
        assert!(emitted[0].pending_json_path.exists());
        // Up SQL contains HeeRanjID install + node seed; no extension section.
        let up = fs::read_to_string(&emitted[0].up_sql_path).unwrap();
        assert!(up.contains("HeeRanjID base schema"));
        assert!(up.contains("ALTER DATABASE \"main\" SET heer.node_id"));
        assert!(!up.contains("CREATE EXTENSION"));
        // Down SQL is comment-only.
        let down = fs::read_to_string(&emitted[0].down_sql_path).unwrap();
        assert!(down.contains("Phase 0 bootstrap — down"));
        assert!(!down.contains("DROP "), "down must not contain real DDL");
        // Pending JSON parses cleanly.
        let pending_bytes = fs::read(&emitted[0].pending_json_path).unwrap();
        let pending: PendingPlan = serde_json::from_slice(&pending_bytes).expect("parse");
        assert_eq!(pending.version, PHASE_ZERO_VERSION);
        assert_eq!(pending.bucket_database, "main");
        assert_eq!(pending.bucket_app, "");
        assert!(pending.checksum_up.starts_with("V1:"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn ensure_phase_zero_idempotent_on_second_run() {
        let (work, guard) = temp_workspace_with_guard("auto_idempotent");
        let apps = vec![AppLifecycle {
            label: String::new(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];
        let models = BTreeMap::new();
        let first =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("first");
        assert_eq!(first.len(), 1);
        let second =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("second");
        assert!(
            second.is_empty(),
            "second run must be a no-op once Phase 0 exists"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn ensure_phase_zero_aggregates_extensions_per_database() {
        use crate::migrate::schema::{
            IndexKindSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
        };

        let (work, guard) = temp_workspace_with_guard("auto_extensions");
        let mk_index = |name: &str, dep: &str| IndexSchema {
            extension_dependency: Some(dep.to_string()),
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::NonUnique,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: "t".to_string(),
            target: IndexTargetSchema::Columns(Vec::new()),
        };
        let mk_schema = |indexes: Vec<IndexSchema>| AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-05-04T00:00:00Z".to_string(),
            indexes,
            models: BTreeMap::new(),
            registered_apps: vec!["billing".to_string()],
        };
        let mut models = BTreeMap::new();
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            mk_schema(vec![mk_index("idx_geom", "postgis")]),
        );
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "shipping".to_string(),
            },
            mk_schema(vec![mk_index("idx_geom2", "postgis")]),
        );
        // Cross-database — should NOT bleed into main's Phase 0.
        models.insert(
            BucketKey {
                database: "crud_log".to_string(),
                app: "audit".to_string(),
            },
            mk_schema(vec![mk_index("idx_text", "pg_trgm")]),
        );

        let apps = vec![
            AppLifecycle {
                label: "billing".to_string(),
                database: "main".to_string(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "shipping".to_string(),
                database: "main".to_string(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "audit".to_string(),
                database: "crud_log".to_string(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let emitted =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("emit");
        assert_eq!(emitted.len(), 2, "one Phase 0 per database");

        // Each database gets only its own extensions; PostGIS-on-main
        // dedups across (billing, shipping); crud_log gets only pg_trgm.
        let main_emit = emitted
            .iter()
            .find(|e| e.database == "main")
            .expect("main emitted");
        assert_eq!(
            main_emit.extensions,
            ["postgis"].iter().map(|s| s.to_string()).collect()
        );
        let crud_emit = emitted
            .iter()
            .find(|e| e.database == "crud_log")
            .expect("crud_log emitted");
        assert_eq!(
            crud_emit.extensions,
            ["pg_trgm"].iter().map(|s| s.to_string()).collect()
        );

        // Up SQL for main contains CREATE EXTENSION postgis but NOT pg_trgm.
        let main_up = fs::read_to_string(&main_emit.up_sql_path).unwrap();
        assert!(main_up.contains("CREATE EXTENSION IF NOT EXISTS \"postgis\""));
        assert!(
            !main_up.contains("\"pg_trgm\""),
            "cross-database extension bled into main"
        );

        // Up SQL for crud_log contains CREATE EXTENSION pg_trgm but NOT postgis.
        let crud_up = fs::read_to_string(&crud_emit.up_sql_path).unwrap();
        assert!(crud_up.contains("CREATE EXTENSION IF NOT EXISTS \"pg_trgm\""));
        assert!(
            !crud_up.contains("\"postgis\""),
            "cross-database extension bled into crud_log"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn ensure_phase_zero_skips_databases_with_existing_marker() {
        let (work, guard) = temp_workspace_with_guard("auto_skip_marker");
        let apps = vec![AppLifecycle {
            label: String::new(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];
        let models = BTreeMap::new();
        // Pre-create all three Phase 0 artifacts for "main" to
        // simulate a complete prior emit. The guard now requires all
        // three to be present — a partial write (only up_path) is no
        // longer treated as complete and will be overwritten.
        let bucket = BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let dir = bucket_dir(&work, &bucket);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(pending_database_dir(&work, "main")).unwrap();
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            "-- existing Phase 0 up",
        )
        .unwrap();
        fs::write(
            dir.join(down_filename(PHASE_ZERO_VERSION)),
            "-- existing Phase 0 down",
        )
        .unwrap();
        fs::write(pending_json_path(&work, &bucket), b"{}").unwrap();

        let emitted =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("emit");
        assert!(
            emitted.is_empty(),
            "main was skipped (all three artifacts present); no other databases in inputs"
        );

        // Confirm the existing up-sql was NOT overwritten.
        let content = fs::read_to_string(dir.join(up_filename(PHASE_ZERO_VERSION))).unwrap();
        assert_eq!(content, "-- existing Phase 0 up");
        let _ = std::fs::remove_dir_all(&work);
    }
}
