//! Bootstrap — production-callable HeeRanjID + Postgres
//! extension installs.
//! # Why this module exists
//! `djogi migrations apply` and `djogi db reset` both need a virgin
//! Postgres database to be brought to a state where any descriptor-driven
//! migration can apply. That state is two pieces:
//! 1. **HeeRanjID schema** — `heerid_next()` / `ranjid_next()` /
//!    the `heer_*` tables / the `current_heer_node_id` GUC reader.
//!    Every model that uses `HeerId` or `RanjId` as primary key
//!    references `DEFAULT heerid_next()` in its `CREATE TABLE` DDL,
//!    so the function must exist before the first descriptor-driven
//!    migration runs.
//! 2. **Postgres extensions declared by descriptors** — `postgis`,
//!    `pgvector`, `pg_trgm`, `btree_gist`, etc. The differ tracks two
//!    descriptor slots:
//! - `IndexSchema::extension_dependency: Option<String>` — set by
//!   the macro / live-migrate patterns for indexes that require a
//!   specific extension (typically `postgis` / `pg_trgm`).
//! - `ExclusionConstraintSchema::extension_dependency: Option<String>`
//!   — auto-derived for `using = "gist"` EXCLUDEs that
//!   mix btree comparison operators (`=`, `<>`, ...) with range or
//!   geometric operators (`&&`, `<<`, ...). Resolves to
//!   `Some("btree_gist")` so the canonical scheduling-style EXCLUDE
//!   `(room_id WITH =, period WITH &&)` auto-installs `btree_gist`
//!   without adopters having to write the `CREATE EXTENSION` SQL.
//!   Before any of these surfaces (spatial / vector / trigram index,
//!   btree_gist-backed EXCLUDE) can be created the matching extension
//!   must be installed.
//!   Pre-Track-0, only the test harness `setup_test_db_with_extensions`
//!   installed these — the CLI / production / example paths hit a virgin
//!   DB and failed on the very first migration that referenced
//!   `DEFAULT heerid_next()`. The example papered over the gap with
//!   hand-rolled `ctx.raw_ddl(...)` for HeeRanjID + PostGIS install.
//!   Lifts that bootstrap into this module:
//! - SQL composition lives in `compose_*` functions that return owned
//!   `String`s. Pure, idempotent, deterministic — re-runs are no-ops.
//! - The runtime driver `run_phase_zero` executes the composed SQL
//!   via a `&tokio_postgres::GenericClient`. Used by both the test
//!   harness (sub-step 0.4) and the auto-emitted migration
//!   that `migrations compose` writes to disk (sub-step 0.3).
//! # Idempotency
//! Every install statement is idempotent in the sense that running
//! Against an already-bootstrapped database is a clean no-op:
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
//!   Re-runs are safe because `db reset` replays every cycle,
//!   and the migration ledger replays once per fresh database.
//! # No regex
//! Per the project-wide no-regex rule, the extension-name validator
//! is implemented with byte-level checks (ASCII letter or underscore
//! followed by ASCII alphanumerics or underscores, up to 63 bytes).
//! See [`validate_extension_name`].
//! # Public surface
//! - [`PHASE_ZERO_VERSION`] — the canonical version label
//!   (`V00000000000000__phase_zero_bootstrap`) the auto-emit path
//!   stamps on the migration. Sorts lexically before any operator-
//!   composed migration (which use `V<YYYYMMDDHHMMSS>__<slug>` with
//!   year ≥ 1000), guaranteeing replay order.
//! - [`DEFAULT_NODE_ID`] — the default node id for single-node
//!   deployments, passed to [`run_phase_zero`] by most callers.
//! - [`run_phase_zero`] — runtime driver that installs HeeRanjID,
//!   required extensions, and, for seed-capable callers, the node-id
//!   GUC in one batch. The only entry point adopters and the test
//!   harness need.
//! - [`BootstrapError`] — error variants surfaced by the runtime
//!   driver.
//! - [`ensure_phase_zero_emitted`] — idempotent per-database
//!   emission called by `migrations compose`. Exposed as `pub` so the
//!   integration test suite can drive it directly.
//!   The SQL composition helpers (`compose_heeranjid_install`,
//!   `compose_extension_installs`, `compose_node_seed`,
//!   `compose_phase_zero`) are `pub(crate)` — used internally
//!   by `migrations compose` and `db reset`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use tokio_postgres::GenericClient;

use super::common;
use super::compose::{AppLifecycle, PENDING_FORMAT_VERSION, PendingPlan, load_pending};
use super::guard::WorkspaceGuard;
use super::ledger::compute_checksum;
use super::naming::{down_filename, up_filename};
use super::projection::BucketKey;
use super::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
use super::target::{
    bucket_dir, pending_database_dir, pending_json_path, phase_zero_pending_json_path,
};

/// Default node id used by single-node deployments.
/// Matches the value `heeranjid::postgres_schema::seed_default_node`
/// inserts and the value the test harness hard-coded pre-Track-0.
/// Multi-node deployments override at the operator layer (separate
/// roadmap item — not in scope for).
pub const DEFAULT_NODE_ID: i32 = 1;

/// Canonical version label for the auto-emitted migration.
/// Uses an all-zero timestamp prefix so it sorts lexically before
/// every operator-composed migration (which use a real timestamp
/// `V<YYYYMMDDHHMMSS>__<slug>` with `YYYY >= 1000`). The runner +
/// `db reset` both replay migrations in lexical version order, so
/// Always lands first on a fresh database.
/// The slug `phase_zero_bootstrap` is reserved — operators cannot
/// compose a migration with this slug and an all-zero timestamp
/// because the version-prefix grammar requires `version_prefix(now)`
/// which always reflects a wall-clock instant.
pub const PHASE_ZERO_VERSION: &str = "V00000000000000__phase_zero_bootstrap";
pub(crate) const PHASE_ZERO_PRODUCTION_BANNER_MARKER: &str =
    "Djogi bootstrap migration — HeeRanjID + extensions";
pub(crate) const PHASE_ZERO_SEEDED_BANNER_MARKER: &str =
    "Djogi bootstrap migration — HeeRanjID + extensions + node seed";
pub(crate) const PHASE_ZERO_BASE_SCHEMA_MARKER: &str =
    "-- HeeRanjID base schema + functions (idempotent).";
pub(crate) const PHASE_ZERO_DEFAULT_NODE_ROW_SEED_MARKER: &str =
    "-- HeeRanjID default-node seed (node_id = 1, ON CONFLICT DO NOTHING).";
pub(crate) const PHASE_ZERO_NODE_SEED_MARKER: &str =
    "-- HeeRanjID node-id GUC seed (database-level + session-level).";

/// Sorted allowlist of Postgres extensions Djogi knows how to install.
/// Validated via `binary_search` in [`compose_extension_installs`]
/// descriptor-derived extension names that are not in this list are
/// rejected before any SQL is emitted. Adding a new extension to
/// Djogi's descriptor system requires a matching entry here.
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
    /// known-extension allowlist ([`ALLOWED_EXTENSIONS`]). Adding new
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
                "bootstrap migration: extension name `{name}` does not match the \
                 Postgres-identifier grammar (ASCII letter or underscore followed \
                 by ASCII alphanumerics or underscores, 1-63 bytes)"
            ),
            BootstrapError::UnknownExtension { name } => write!(
                f,
                "bootstrap migration: extension `{name}` is not in Djogi's \
                 known-extension allowlist; add it to ALLOWED_EXTENSIONS in \
                 migrate/bootstrap.rs to enable installation"
            ),
            BootstrapError::Db { step, source } => {
                write!(f, "bootstrap migration: {step} failed: {source}")
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
/// Includes the base `INSTALL_SQL` (schema + session helpers +
/// `generate_id` + `generate_ranj_id`), the v0.3 desc-support
/// primitives (`heerid_to_desc`, `*_next_desc`, bulk backfill), and
/// optionally the default-node seed when `include_seed` is `true`.
/// All of these are idempotent — re-running against an already-
/// installed database is a no-op.
///
/// **Seed inclusion.** Production/cluster Phase 0 should NOT
/// unconditionally seed node 1 into `heer_nodes`; node registration
/// is an operator provisioning step. Migration-file emit passes
/// `include_seed: false` so the canonical Phase 0 SQL stays
/// identity-free. Seed-capable direct callers and tests can pass
/// `include_seed: true` when they intentionally want the helper to
/// install the default node row in the same batch.
///
/// Returns an owned `String` so the caller can hash it into the
/// migration's checksum, write it to disk verbatim, or feed it to
/// `client.batch_execute` directly.
/// **Why this function** rather than calling
/// `heeranjid::postgres_schema::install_schema` etc. directly: the
/// auto-emit path needs the SQL as a `String` to write into the
/// `<workspace>/migrations/<db>/<app>/V00000000000000__phase_zero_bootstrap.sdjql`
/// file. The runtime test-harness path also benefits — a single
/// composed blob means one `batch_execute` call with one round-trip,
/// instead of four.
pub(crate) fn compose_heeranjid_install(include_seed: bool) -> String {
    // The order here mirrors what the test harness ran pre-Track-0:
    // base install, desc-support primitives, seed (optional).
    // Each blob from heeranjid is already a self-contained CREATE
    // OR REPLACE / CREATE IF NOT EXISTS / ON CONFLICT DO NOTHING
    // sequence. We concatenate with explicit blank lines + section
    // comments so the on-disk migration file is readable.
    let seed_len = if include_seed {
        heeranjid::postgres_schema::SEED_SQL.len() + 80
    } else {
        0
    };
    let mut out = String::with_capacity(
        heeranjid::postgres_schema::INSTALL_SQL.len()
            + heeranjid::postgres_schema::DESC_FLIP_SQL.len()
            + heeranjid::postgres_schema::DESC_GENERATORS_SQL.len()
            + heeranjid::postgres_schema::BULK_BACKFILL_SQL.len()
            + seed_len,
    );
    out.push_str(PHASE_ZERO_BASE_SCHEMA_MARKER);
    out.push('\n');
    out.push_str(heeranjid::postgres_schema::INSTALL_SQL);
    out.push_str("\n\n-- HeeRanjID desc-flip primitives (heerid_to_desc / ranjid_to_desc / heerid_flip_mask).\n");
    out.push_str(heeranjid::postgres_schema::DESC_FLIP_SQL);
    out.push_str("\n\n-- HeeRanjID single-row generators + *_next_desc generators.\n");
    out.push_str(heeranjid::postgres_schema::DESC_GENERATORS_SQL);
    out.push_str("\n\n-- HeeRanjID migration-support procedures (bulk backfill).\n");
    out.push_str(heeranjid::postgres_schema::BULK_BACKFILL_SQL);
    if include_seed {
        out.push_str("\n\n");
        out.push_str(PHASE_ZERO_DEFAULT_NODE_ROW_SEED_MARKER);
        out.push('\n');
        out.push_str(heeranjid::postgres_schema::SEED_SQL);
    }
    out
}

/// Compose `CREATE EXTENSION IF NOT EXISTS "<name>"` statements for
/// each entry in the supplied set, one per line, in sorted order.
/// Names are validated against the Postgres-identifier grammar before
/// any output is emitted; an invalid name surfaces as
/// [`BootstrapError::InvalidExtensionName`] with the offending
/// value preserved for the operator message. Validation runs first
/// so the caller can fail fast — partial output is never produced.
/// `IF NOT EXISTS` makes the statement idempotent; re-running against
/// an already-installed extension is a no-op. Names are double-quoted
/// so the SQL is safe even if a future extension name were to collide
/// with a Postgres keyword.
/// An empty set returns an empty string. Callers concatenate this
/// into [`compose_phase_zero`] without conditional handling.
pub(crate) fn compose_extension_installs(
    extensions: &BTreeSet<String>,
) -> Result<String, BootstrapError> {
    // Two-pass validation: identifier grammar first (cheap), then
    // allowlist (binary_search on ALLOWED_EXTENSIONS). Both passes
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

/// Compose the node-id seed SQL — runtime database-level defaults for
/// new connections plus session-level `SET`s so the applying
/// connection sees the value immediately.
///
/// Seeds **both** `heer.node_id` (consumed by `heerid_next()` via
/// `current_heer_node_id()`) and `heer.ranj_node_id` (consumed by
/// `ranjid_next()` via `current_heer_ranj_node_id()`). HeeRanjID
/// stores these as two separate session GUCs because the underlying
/// node-id ranges differ (HeerId: 0..=511, RanjId: 0..=32767), but a
/// single-node deployment uses the same logical node id for both
/// the `seed.sql` from heeranjid pre-populates rows in both
/// `heer_node_state` and `heer_ranj_node_state` for that node id, so
/// pointing both GUCs at it is the only way `ranjid_next()` works out
/// of the box. Multi-node operators that want different ids per
/// generator must override the Phase 0 SQL.
///
/// The emitted batch is idempotent: re-running with the same value is
/// a metadata-only no-op on the database side and a session-write
/// no-op on the client side.
///
/// **Why both** runtime `ALTER DATABASE` and a session SET: the Phase 0
/// SQL runs through the current runner connection. The runtime
/// `ALTER DATABASE` persists the default for whichever physical
/// database is currently connected, without baking a logical or
/// physical name into the persisted SQL. The session-level SET covers
/// the running connection itself — without it, an additive migration
/// applied immediately after Phase 0 in the same `apply` run would
/// lack the GUC and
/// `current_heer_node_id()` / `current_heer_ranj_node_id()` would
/// raise.
///
/// `node_id` must be a non-negative `i32`; the SQL uses the raw
/// integer (no quoting) which is safe because the type is integer-
/// only. HeeRanjID's `set_heer_node_id` / `set_heer_ranj_node_id`
/// enforce per-generator range bounds at the SQL layer if a caller
/// passes the value out of range later; the seed here only writes
/// the GUC literal.
///
/// The `database` argument is retained for surrounding composition
/// compatibility, but the node-seed SQL never splices it into
/// persisted `ALTER DATABASE` statements.
pub(crate) fn compose_node_seed(_database: &str, node_id: i32) -> Result<String, BootstrapError> {
    let node_id_str = node_id.to_string();
    let mut out = String::with_capacity(node_id_str.len() * 6 + 384);
    out.push_str(PHASE_ZERO_NODE_SEED_MARKER);
    out.push('\n');
    out.push_str(
        "-- `heer.node_id` powers heerid_next(); `heer.ranj_node_id` powers ranjid_next().\n",
    );
    out.push_str("DO $djogi$\n");
    out.push_str("BEGIN\n");
    out.push_str(
        "    EXECUTE format('ALTER DATABASE %I SET heer.node_id = %L', current_database(), '",
    );
    out.push_str(&node_id_str);
    out.push_str("');\n");
    out.push_str(
        "    EXECUTE format('ALTER DATABASE %I SET heer.ranj_node_id = %L', current_database(), '",
    );
    out.push_str(&node_id_str);
    out.push_str("');\n");
    out.push_str("END\n");
    out.push_str("$djogi$;\n");
    out.push_str("SET heer.node_id = '");
    out.push_str(&node_id_str);
    out.push_str("';\n");
    out.push_str("SET heer.ranj_node_id = '");
    out.push_str(&node_id_str);
    out.push_str("';\n");
    Ok(out)
}

/// Compose the complete Phase 0 SQL — HeeRanjID install + extensions
/// + optionally node seed, in dependency order.
///   Consumers:
/// - `migrations compose` writes this to
///   `<workspace>/migrations/<db>/<app>/V00000000000000__phase_zero_bootstrap.sdjql`
///   and tracks it in the ledger like any other migration.
/// - The test harness `setup_test_db_with_extensions` runs this
///   directly via [`run_phase_zero`] before applying pending
///   migrations.
///   Order matters: HeeRanjID schema must exist before any extension
///   install runs (in case an extension's setup script touches the
///   `heer` schema), and both must exist before the node seed runs (in
///   case the seed relies on extension-provided types).
///
/// **Node seed inclusion.** Production/cluster Phase 0 should NOT
/// include a node seed. Pass `include_node_seed: false` for production
/// emit — the resulting on-disk SQL installs only the HeeRanjID schema,
/// functions, and extensions without baking any node identity into the
/// persisted file.
///
/// Seed-capable direct callers can pass `include_node_seed: true`; this
/// includes both the `heer_nodes` seed row (from [`compose_heeranjid_install`])
/// and the database-level GUC defaults + session SETs (from
/// [`compose_node_seed`]). Migration-file emit keeps this `false` for
/// both production and `--single-node-dev`; the runner performs explicit
/// dev provisioning after Phase 0 SQL succeeds.
///
///   Returns owned bytes so the caller can hash, write, or execute
///   directly.
pub(crate) fn compose_phase_zero(
    database: &str,
    extensions: &BTreeSet<String>,
    node_id: i32,
    include_node_seed: bool,
) -> Result<String, BootstrapError> {
    let heeranjid = compose_heeranjid_install(include_node_seed);
    let exts = compose_extension_installs(extensions)?;
    let mut out = String::with_capacity(heeranjid.len() + exts.len() + 256);

    // Banner text depends on whether node seed is included.
    if include_node_seed {
        out.push_str("-- ╭────────────────────────────────────────────────────────────────╮\n");
        out.push_str("-- │ ");
        out.push_str(PHASE_ZERO_SEEDED_BANNER_MARKER);
        out.push_str(" │\n");
        out.push_str("-- │ Auto-emitted by `djogi migrations compose`. Idempotent.        │\n");
        out.push_str("-- ╰────────────────────────────────────────────────────────────────╯\n\n");
    } else {
        out.push_str("-- ╭───────────────────────────────────────────────────────────────╮\n");
        out.push_str("-- │ ");
        out.push_str(PHASE_ZERO_PRODUCTION_BANNER_MARKER);
        out.push_str("            │\n");
        out.push_str("-- │ Auto-emitted by `djogi migrations compose`. Idempotent.       │\n");
        out.push_str("-- ╰───────────────────────────────────────────────────────────────╯\n\n");
    }

    out.push_str(&heeranjid);
    if !exts.is_empty() {
        out.push_str("\n\n");
        out.push_str(&exts);
    }
    if include_node_seed {
        let node = compose_node_seed(database, node_id)?;
        out.push_str("\n\n");
        out.push_str(&node);
    }
    Ok(out)
}

// ── Runtime driver ────────────────────────────────────────────────────────

/// Execute Phase 0 SQL against a live Postgres connection.
/// Used by the test harness (sub-step 0.4) and any production caller
/// that wants to bring a fresh database to a Phase-0 state without
/// going through the on-disk migration ledger (e.g. one-shot
/// provisioning scripts).
/// **Single batch.** The composed SQL is sent through `batch_execute`
/// in one round-trip. Postgres parses + executes the whole batch
/// inside an implicit transaction (no `BEGIN`/`COMMIT` in the SQL),
/// so a partial failure rolls back cleanly and leaves the database
/// at its prior state.
/// **Idempotent.** See module-level idempotency notes — every
/// statement uses `CREATE OR REPLACE`, `IF NOT EXISTS`, or
/// `ON CONFLICT DO NOTHING` so re-running against an already-
/// bootstrapped database is a no-op.
///
/// **Node seed inclusion.** When `include_node_seed` is `true`, the
/// composed SQL includes both the HeeRanjID default-node seed row
/// and the database-level GUC defaults + session SETs from
/// [`compose_node_seed`]. Migration runner callers should pass
/// `include_node_seed: false` so that node registration remains an
/// explicit provisioning step; `SingleNodeDev` provisioning runs
/// after Phase 0 SQL succeeds.
///
/// `node_id` is only used when `include_node_seed` is `true`; it
/// defaults to [`DEFAULT_NODE_ID`] (1) for single-node deployments.
/// # Errors
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
    include_node_seed: bool,
) -> Result<(), BootstrapError>
where
    C: GenericClient + ?Sized,
{
    let sql = compose_phase_zero(database, extensions, node_id, include_node_seed)?;
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
/// across every index AND every exclusion constraint in every model in
/// the per-bucket `AppliedSchema` map (all databases combined).
/// Production compose uses [`extensions_for_database`] to aggregate
/// per-database. This cross-database variant exists for unit tests
/// that verify deduplication semantics across multiple buckets.
/// **Source slots walked.** Two slots feed the result:
/// 1. `AppliedSchema::indexes[i].extension_dependency` — set by the
///    macro / live-migrate patterns when an index requires a specific
///    extension (`postgis` for GiST-on-geography, `pg_trgm` for trigram
///    indexes, etc.).
/// 2. `AppliedSchema::models[*].exclusion_constraints[i].extension_dependency`
///    set by the macro auto-derivation under when a
///    `using = "gist"` EXCLUDE mixes btree comparison operators with
///    range / geometric operators. Resolves to `Some("btree_gist")`.
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
        for table in schema.models.values() {
            for excl in &table.exclusion_constraints {
                if let Some(dep) = &excl.extension_dependency {
                    deps.insert(dep.clone());
                }
            }
        }
    }
    deps
}

// ── Auto-emit (sub-step 0.3) ──────────────────────────────────────────────

/// One database that received a emission during this compose
/// run. Returned in [`EmittedPhaseZero`] reports so the CLI can log
/// structured progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedPhaseZero {
    /// Database name. is emitted into the synthetic
    /// `_global_` bucket of this database.
    pub database: String,
    /// Final on-disk path of the up-side migration file.
    pub up_sql_path: PathBuf,
    /// Final on-disk path of the down-side migration file.
    pub down_sql_path: PathBuf,
    /// Final on-disk path of the pending JSON file.
    pub pending_json_path: PathBuf,
    /// Distinct extensions baked into this emission. Useful
    /// for logging — adopters often want to see which Postgres
    /// extensions were detected.
    pub extensions: BTreeSet<String>,
}

/// Errors surfaced by [`ensure_phase_zero_emitted`].
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
            AutoEmitError::Compose(e) => write!(f, "bootstrap migration auto-emit: {e}"),
            AutoEmitError::Io { path, source } => write!(
                f,
                "bootstrap migration auto-emit: i/o failure at {}: {source}",
                path.display()
            ),
            AutoEmitError::PendingJson(e) => {
                write!(
                    f,
                    "bootstrap migration auto-emit: pending JSON serialization: {e}"
                )
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
/// Bootstrap migration for any database that doesn't already
/// have one on disk.
/// **When this fires.** A database is considered "missing "
/// when the file
/// `<workspace>/migrations/<database>/_global_/V00000000000000__phase_zero_bootstrap.sdjql`
/// does not exist. Once it exists, subsequent compose runs leave it
/// untouched (idempotent — running compose twice never re-emits).
/// **What gets baked in.** The SQL composition (per
/// [`compose_phase_zero`]) snapshots the descriptor inventory's
/// extension dependencies AT THE TIME of the first compose. Future
/// compose runs that introduce NEW extensions (e.g. adding a
/// `pgvector` index after PostGIS was already in use) emit a regular
/// migration via the standard delta path — they do NOT re-emit
/// . The trade-off: simpler invariant (is one-shot)
/// at the cost of a separate `CREATE EXTENSION` migration for any
/// extension added later.
/// **Bucket placement.** lives in the synthetic global
/// bucket (empty-string app label) of each database, on disk at
/// `migrations/<database>/_global_/`. This means the auto-emit fires
/// once per `(database, "")` pair and the regular `_global_` app
/// space carries one extra "fixed point" migration that always
/// applies first.
/// **Down side.** is bootstrap — there is no meaningful
/// rollback. The down-side file is comment-only with an explicit
/// "bootstrap migration has no rollback" marker. `db reset` never invokes the
/// down side; this exists only to satisfy the migration-pair
/// convention and keep tooling that reads the down side simple.
/// **Pending JSON.** The pending JSON tracks the same way as
/// any composed migration. Its `model_snapshot` is the empty schema
/// because doesn't define any user-visible schema — it only
/// installs framework dependencies.
/// **Idempotency contract.**
/// - Running compose against a workspace that already has
///   on disk is a no-op for the path. Returns
///   `Ok(Vec::new())`.
/// - Running compose against a workspace with NO yet emits
///   exactly one per database in the inputs.
///   **Witness-typed lock.** The `_guard: &WorkspaceGuard` parameter
///   is a compile-time witness that the workspace lock is held — the
///   same convention compose / runner use. The function does not
///   touch the guard; the parameter is named with a leading underscore
///   to signal "consumed at the type level only".
pub fn ensure_phase_zero_emitted(
    workspace_root: &Path,
    models: &BTreeMap<BucketKey, AppliedSchema>,
    apps: &[AppLifecycle],
    now: OffsetDateTime,
    _guard: &WorkspaceGuard,
) -> Result<Vec<EmittedPhaseZero>, AutoEmitError> {
    let workspace_root_canon = workspace_root
        .canonicalize()
        .map_err(|e| AutoEmitError::Io {
            path: workspace_root.to_path_buf(),
            source: e,
        })?;

    // 1. Collect the distinct database set from inputs.
    // Sources:
    // - Every bucket in `models` carries a `database`.
    // - Every entry in `apps` carries a `database` — covers the
    // case where an app is registered but has no models yet
    // (still needs its database bootstrapped).
    let mut databases: BTreeSet<String> = BTreeSet::new();
    for bucket in models.keys() {
        databases.insert(bucket.database.clone());
    }
    for app in apps {
        databases.insert(app.database.clone());
    }

    // 2. For each database, decide whether to emit. Skip databases
    // that already have on disk.
    let mut emitted: Vec<EmittedPhaseZero> = Vec::new();
    for database in &databases {
        let bucket = BucketKey {
            database: database.clone(),
            app: String::new(),
        };
        let dir = bucket_dir(workspace_root, &bucket);
        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let down_path = dir.join(down_filename(PHASE_ZERO_VERSION));
        let pending_path =
            phase_zero_pending_json_path(workspace_root, database, PHASE_ZERO_VERSION);
        let legacy_pending_path = pending_json_path(workspace_root, &bucket);

        // Validate constructed paths stay within the canonicalized workspace root
        // to prevent path-injection via symlinks or relative components.
        if common::ensure_within_base(&workspace_root_canon, &dir).is_err()
            || common::ensure_within_base(&workspace_root_canon, &up_path).is_err()
            || common::ensure_within_base(&workspace_root_canon, &down_path).is_err()
            || common::ensure_within_base(&workspace_root_canon, &pending_path).is_err()
            || common::ensure_within_base(&workspace_root_canon, &legacy_pending_path).is_err()
        {
            continue;
        }

        let hidden_pending_exists = pending_path.exists();

        // All three artifacts must be present and the pending JSON must
        // parse as a Phase 0 witness before the emit is considered
        // complete. Checking only path existence would let partial or
        // foreign pending files suppress re-emission forever.
        let phase_zero_complete = up_path.exists()
            && down_path.exists()
            && load_pending(&pending_path)
                .ok()
                .is_some_and(|plan| phase_zero_pending_matches(&plan, database));
        let legacy_phase_zero_complete = !hidden_pending_exists
            && up_path.exists()
            && down_path.exists()
            && load_pending(&legacy_pending_path)
                .ok()
                .is_some_and(|plan| phase_zero_pending_matches(&plan, database));
        if phase_zero_complete || legacy_phase_zero_complete {
            continue;
        }

        // 3. Compose the SQL for this database.
        // Aggregate extensions from EVERY bucket in this database
        // (per-bucket `app` is irrelevant — extensions are a
        // Postgres-cluster-level concept and PostGIS-on-billing is
        // the same install as PostGIS-on-shipping).
        let extensions = extensions_for_database(models, database);
        // Production emit: no node seed. The on-disk Phase 0 SQL
        // installs only HeeRanjID schema/functions/extensions without
        // baking any node identity. Node seeding is handled at runtime
        // by the runner for explicit --single-node-dev mode.
        let up_sql = compose_phase_zero(database, &extensions, DEFAULT_NODE_ID, false)?;
        let down_sql = compose_phase_zero_down_text();

        // 4. Build the pending JSON. `model_snapshot` is empty
        // doesn't capture any user schema, only framework
        // bootstrap. Production callers re-derive snapshots
        // on subsequent composes; the row in the ledger
        // survives the snapshot rebuild.
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
            depends_on: Vec::new(),
        };
        let pending_bytes =
            serde_json::to_vec_pretty(&pending).map_err(AutoEmitError::PendingJson)?;

        // 5. Ensure parent directories exist for the bucket dir and
        // the pending dir, then write all three files.
        // These calls are placed after the containment checks above so
        // no filesystem operations run on unvalidated paths.
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

fn phase_zero_pending_matches(plan: &PendingPlan, database: &str) -> bool {
    plan.bucket_database == database
        && plan.bucket_app.is_empty()
        && plan.version == PHASE_ZERO_VERSION
}

/// Slug component of [`PHASE_ZERO_VERSION`]. Exposed so consumers
/// that need to construct the version string from parts (rare) can
/// stay aligned with the canonical label.
const PHASE_ZERO_SLUG: &str = "phase_zero_bootstrap";

/// Compose the down-side SQL — comment-only.
/// Has no meaningful rollback: dropping the HeeRanjID schema
/// would invalidate every model's primary key, dropping PostGIS
/// would invalidate every spatial column. `db reset` re-replays
/// From scratch on the recreated database; rollback through
/// the down side is not a supported flow.
/// We emit a comment block rather than an empty file so tooling that
/// reads the down side (status reports, diff views) sees an explicit
/// "no rollback" message rather than a silent empty file that might
/// be mistaken for missing or corrupt.
fn compose_phase_zero_down_text() -> String {
    let mut out = String::with_capacity(512);
    out.push_str("-- Djogi bootstrap migration — down (no-op).\n");
    out.push_str("--\n");
    out.push_str(
        "-- The bootstrap migration installs framework dependencies (HeeRanjID schema +\n",
    );
    out.push_str("-- Postgres extensions) that every subsequent\n");
    out.push_str("-- migration depends on. Rolling those back would invalidate the\n");
    out.push_str("-- entire schema, so the bootstrap migration has no meaningful down side.\n");
    out.push_str("--\n");
    out.push_str("-- `djogi db reset` re-replays the bootstrap migration from scratch on the\n");
    out.push_str(
        "-- recreated database. The migration ledger tracks the bootstrap migration like\n",
    );
    out.push_str("-- any other migration; rolling it back is not a supported flow.\n");
    out
}

/// Aggregate extension dependencies from every bucket in a single
/// database.
/// PostGIS declared in `main.billing` and `main.shipping` merges to
/// one install in `main`'s . PostGIS declared in
/// `crud_log.audit` lives in a separate for the `crud_log`
/// database.
/// **Source slots walked.** Two slots feed the result:
/// 1. `AppliedSchema::indexes[i].extension_dependency` — set when an
///    index requires a specific extension (`postgis` for GiST-on-
///    geography, `pg_trgm` for trigram indexes, etc.).
/// 2. `AppliedSchema::models[*].exclusion_constraints[i].extension_dependency`
///    set by the macro auto-derivation under when a
///    `using = "gist"` EXCLUDE mixes btree comparison operators with
///    range / geometric operators. Resolves to `Some("btree_gist")`.
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
        // exclusion constraints can carry their own
        // extension dependency (typically `btree_gist`) auto-derived by
        // the macro. Aggregate alongside index dependencies so the
        // Bootstrap migration installs every required extension
        // in one place.
        for table in schema.models.values() {
            for excl in &table.exclusion_constraints {
                if let Some(dep) = &excl.extension_dependency {
                    deps.insert(dep.clone());
                }
            }
        }
    }
    deps
}

/// Ensure the parent directory of `path` exists; create it (and any
/// missing intermediates) when absent. Mirrors the helper used in
/// `compose.rs` — duplicated here so `bootstrap` does not import
/// internals from a peer module.
///
/// Canonicalizes the resolved parent path before creating directories,
/// so symlinks in the caller-supplied path cannot redirect the operation
/// to an arbitrary location.
fn ensure_parent(path: &Path) -> Result<(), AutoEmitError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        // Canonicalize the parent (or its existing ancestor) so symlinks
        // cannot redirect the directory creation. If the parent does not
        // yet exist, canonicalize the deepest existing ancestor and
        // re-append the remaining components.
        let parent_canon = if parent.exists() {
            parent.canonicalize().map_err(|e| AutoEmitError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?
        } else {
            let mut existing = PathBuf::from(parent);
            loop {
                match existing.canonicalize() {
                    Ok(base) => {
                        let suffix =
                            parent
                                .strip_prefix(&existing)
                                .map_err(|_| AutoEmitError::Io {
                                    path: parent.to_path_buf(),
                                    source: io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        "parent path cannot be resolved",
                                    ),
                                })?;
                        break base.join(suffix);
                    }
                    Err(_) => {
                        match existing.parent() {
                            Some(p) if !p.as_os_str().is_empty() => {
                                existing = PathBuf::from(p);
                            }
                            _ => {
                                // Reached root without finding an existing ancestor.
                                // Fallback to the original parent path.
                                break parent.to_path_buf();
                            }
                        }
                    }
                }
            }
        };
        fs::create_dir_all(&parent_canon).map_err(|e| AutoEmitError::Io {
            path: parent_canon,
            source: e,
        })?;
    }
    Ok(())
}

/// Build an empty [`AppliedSchema`] for the supplied bucket — used as
/// the pending JSON's `model_snapshot` since captures
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
/// so the pending JSON's `composed_at` field matches the
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
/// statement.
///
/// Rules (byte-level, no regex per the Djogi-wide no-regex policy):
/// - Length between 1 and 63 bytes inclusive (Postgres `NAMEDATALEN`
///   minus the trailing `NUL`).
/// - First byte is an ASCII letter (upper- or lower-case) or
///   underscore.
/// - Every subsequent byte is an ASCII letter, digit, or underscore.
///   All real-world extension names on PGXN and in `contrib/`
///   (`postgis`, `pg_trgm`, `pgcrypto`, `pgvector`, `vector`) match
///   this rule.
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
    fn compose_node_seed_uses_current_database_for_defaults_and_keeps_session_set() {
        let sql = compose_node_seed("djogi_test_abc", 7).unwrap();
        // Both GUCs are seeded at both scopes: HeerId path (heer.node_id)
        // and RanjId path (heer.ranj_node_id) need separate session
        // variables. See compose_node_seed for the per-generator
        // rationale.
        assert!(sql.contains("current_database()"));
        assert!(!sql.contains("ALTER DATABASE \"djogi_test_abc\""));
        assert!(sql.contains("SET heer.node_id = '7'"));
        assert!(sql.contains("SET heer.ranj_node_id = '7'"));
    }

    #[test]
    fn compose_node_seed_does_not_splice_the_database_argument() {
        let sql = compose_node_seed("bad name", 1).unwrap();
        assert!(sql.contains("current_database()"));
        assert!(!sql.contains("bad name"));
    }

    #[test]
    fn compose_heeranjid_install_includes_all_blobs_with_seed() {
        let sql = compose_heeranjid_install(true);
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
    fn compose_heeranjid_install_omits_seed_for_production() {
        let sql = compose_heeranjid_install(false);
        // Core sections are always present.
        assert!(sql.contains("HeeRanjID base schema"));
        assert!(sql.contains("desc-flip primitives"));
        assert!(sql.contains("single-row generators"));
        assert!(sql.contains("bulk backfill"));
        // Seed section is absent in production mode.
        assert!(!sql.contains("default-node seed"));
        assert!(!sql.contains(heeranjid::postgres_schema::SEED_SQL));
    }

    #[test]
    fn compose_phase_zero_orders_install_then_extensions_then_seed() {
        let mut exts = BTreeSet::new();
        exts.insert("postgis".to_string());
        // Dev mode: seed is included and ordered after extensions.
        let sql = compose_phase_zero("djogi_test_db", &exts, 1, true).unwrap();
        let install_idx = sql.find("HeeRanjID base schema").expect("install present");
        let ext_idx = sql.find("CREATE EXTENSION").expect("extension present");
        let seed_idx = sql.find("current_database()").expect("seed present");
        assert!(install_idx < ext_idx, "install must precede extensions");
        assert!(ext_idx < seed_idx, "extensions must precede node seed");
    }

    #[test]
    fn compose_phase_zero_production_omits_node_seed() {
        let mut exts = BTreeSet::new();
        exts.insert("postgis".to_string());
        let sql = compose_phase_zero("djogi_test_db", &exts, 1, false).unwrap();
        // Core sections present.
        assert!(sql.contains("HeeRanjID base schema"));
        assert!(sql.contains("CREATE EXTENSION"));
        // No node seed in production mode.
        assert!(!sql.contains("current_database()"));
        assert!(!sql.contains("SET heer.node_id"));
        assert!(!sql.contains("default-node seed"));
    }

    #[test]
    fn compose_phase_zero_omits_extension_section_when_empty() {
        let exts = BTreeSet::new();
        let sql = compose_phase_zero("djogi_test_db", &exts, 1, true).unwrap();
        assert!(!sql.contains("CREATE EXTENSION"));
        assert!(sql.contains("current_database()"));
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

    /// Build an `AppliedSchema` carrying one table with one
    /// `ExclusionConstraintSchema` whose `extension_dependency` is the
    /// supplied value. Used by the aggregation tests below.
    #[cfg(test)]
    fn schema_with_exclusion(
        table: &str,
        constraint_name: &str,
        ext: Option<&str>,
    ) -> AppliedSchema {
        use crate::migrate::schema::{
            ExclusionConstraintSchema, ExclusionElementSchema, PkKindSchema, PrimaryKeySchema,
            TableSchema,
        };

        let mut models = BTreeMap::new();
        models.insert(
            table.to_string(),
            TableSchema {
                app: None,
                columns: Vec::new(),
                exclusion_constraints: vec![ExclusionConstraintSchema {
                    deferrable: false,
                    elements: vec![ExclusionElementSchema {
                        expr: "x".to_string(),
                        with_operator: "=".to_string(),
                    }],
                    extension_dependency: ext.map(|s| s.to_string()),
                    initially_deferred: false,
                    name: constraint_name.to_string(),
                    using: "gist".to_string(),
                    where_clause: None,
                }],
                fts: None,
                is_through: false,
                moved_from_app: None,
                partition: None,
                primary_key: PrimaryKeySchema {
                    columns: vec!["id".to_string()],
                    kind: PkKindSchema::HeerId,
                },
                rationale: None,
                renamed_from: None,
                rls_enabled: false,
                storage_params: None,
                table: table.to_string(),
                table_comment: None,
                tablespace: None,
                tenant_key: None,
            },
        );

        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: super::super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-05-18T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models,
            registered_apps: vec!["".to_string()],
        }
    }

    /// exclusion-constraint `extension_dependency` slots feed
    /// the cross-bucket aggregator alongside `IndexSchema` slots. Both
    /// `btree_gist` (from EXCLUDE) and `postgis` (from an index) survive
    /// the merge in deduplicated form.
    #[test]
    fn extension_dependencies_from_models_includes_exclusion_constraints() {
        use crate::migrate::schema::{
            IndexKindSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
        };

        let mut models = BTreeMap::new();
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            schema_with_exclusion("invoices", "no_overlap_inv", Some("btree_gist")),
        );

        // Different bucket, same database — adds an INDEX-derived
        // PostGIS dependency. Verify both sources merge into one set.
        let mut shipping_schema = AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: super::super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-05-18T00:00:00Z".to_string(),
            indexes: vec![IndexSchema {
                extension_dependency: Some("postgis".to_string()),
                include: Vec::new(),
                index_type: IndexTypeSchema::Gist,
                kind: IndexKindSchema::NonUnique,
                name: "ship_geom_idx".to_string(),
                nulls_not_distinct: false,
                predicate: None,
                requires_out_of_transaction: false,
                table: "shipments".to_string(),
                target: IndexTargetSchema::Columns(Vec::new()),
            }],
            models: BTreeMap::new(),
            registered_apps: vec!["shipping".to_string()],
        };
        // Add a duplicate btree_gist EXCLUDE in a different bucket to
        // exercise dedup across buckets.
        shipping_schema.models.extend(
            schema_with_exclusion("appointments", "no_overlap_appt", Some("btree_gist")).models,
        );
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "shipping".to_string(),
            },
            shipping_schema,
        );

        let deps = extension_dependencies_from_models(&models);
        let expected: BTreeSet<String> = ["btree_gist", "postgis"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            deps, expected,
            "btree_gist (from EXCLUDE) and postgis (from index) must merge",
        );
    }

    /// `None` extension dependency on an EXCLUDE is treated
    /// as a no-op signal (matching the IndexSchema behaviour). Pure-
    /// range exclusions skip the install entirely.
    #[test]
    fn extension_dependencies_from_models_skips_none_exclusion_deps() {
        let mut models = BTreeMap::new();
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            schema_with_exclusion("period_only", "no_overlap_period", None),
        );

        let deps = extension_dependencies_from_models(&models);
        assert!(
            deps.is_empty(),
            "EXCLUDE with extension_dependency=None must not request any install: {deps:?}",
        );
    }

    /// `extensions_for_database` (production helper) walks
    /// exclusion constraints in the named database AND skips buckets in
    /// other databases. Mirrors the per-database isolation tested for
    /// index-driven dependencies above.
    #[test]
    fn extensions_for_database_includes_exclusion_constraints_per_database() {
        let mut models = BTreeMap::new();
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            schema_with_exclusion("bookings", "no_overlap", Some("btree_gist")),
        );
        // crud_log carries an EXCLUDE that needs btree_gist for its
        // OWN database — it must NOT bleed into `main`.
        models.insert(
            BucketKey {
                database: "crud_log".to_string(),
                app: "audit".to_string(),
            },
            schema_with_exclusion("audit_periods", "no_audit_overlap", Some("btree_gist")),
        );

        let main_deps = extensions_for_database(&models, "main");
        assert_eq!(
            main_deps,
            ["btree_gist"].iter().map(|s| s.to_string()).collect(),
        );
        let crud_deps = extensions_for_database(&models, "crud_log");
        assert_eq!(
            crud_deps,
            ["btree_gist"].iter().map(|s| s.to_string()).collect(),
        );
        let event_deps = extensions_for_database(&models, "event_log");
        assert!(
            event_deps.is_empty(),
            "unreferenced database must have no extension installs: {event_deps:?}",
        );
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
        // Canonicalize temp_dir and validate containment before creating
        // directories, so symlinks cannot redirect the workspace outside
        // the temp directory.
        let temp_dir_canon = std::env::temp_dir()
            .canonicalize()
            .expect("temp dir exists");
        if !root.starts_with(&temp_dir_canon) {
            panic!("test workspace path escaped temp directory");
        }
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
        assert_eq!(emitted.len(), 1, "one bootstrap migration per database");
        assert_eq!(emitted[0].database, "main");
        assert!(emitted[0].extensions.is_empty(), "no extensions in models");
        assert!(emitted[0].up_sql_path.exists());
        assert!(emitted[0].down_sql_path.exists());
        assert!(emitted[0].pending_json_path.exists());
        // Up SQL contains HeeRanjID install; no node seed in production emit.
        assert!(emitted[0].up_sql_path.starts_with(&work));
        let up = fs::read_to_string(&emitted[0].up_sql_path).unwrap();
        assert!(up.contains("HeeRanjID base schema"));
        // Production emit: no node-seed section, no database-level defaults.
        assert!(!up.contains("current_database()"));
        assert!(!up.contains("SET heer.node_id"));
        assert!(!up.contains("default-node seed"));
        assert!(!up.contains("CREATE EXTENSION"));
        // Down SQL is comment-only.
        assert!(emitted[0].down_sql_path.starts_with(&work));
        let down = fs::read_to_string(&emitted[0].down_sql_path).unwrap();
        assert!(down.contains("bootstrap migration — down"));
        assert!(!down.contains("DROP "), "down must not contain real DDL");
        // Pending JSON parses cleanly.
        assert!(emitted[0].pending_json_path.starts_with(&work));
        let pending_bytes = fs::read(&emitted[0].pending_json_path).unwrap();
        let pending: PendingPlan = serde_json::from_slice(&pending_bytes).expect("parse");
        assert_eq!(pending.version, PHASE_ZERO_VERSION);
        assert_eq!(pending.bucket_database, "main");
        assert_eq!(pending.bucket_app, "");
        assert!(pending.checksum_up.starts_with("V1:"));
        let temp_canon = std::env::temp_dir().canonicalize().unwrap_or_default();
        if work.starts_with(&temp_canon) {
            let _ = std::fs::remove_dir_all(&work);
        }
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
            "second run must be a no-op once the bootstrap migration exists"
        );
        let temp_canon = std::env::temp_dir().canonicalize().unwrap_or_default();
        if work.starts_with(&temp_canon) {
            let _ = std::fs::remove_dir_all(&work);
        }
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
        // Cross-database — should NOT bleed into main's .
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
        assert_eq!(emitted.len(), 2, "one bootstrap migration per database");

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
        assert!(main_emit.up_sql_path.starts_with(&work));
        let main_up = fs::read_to_string(&main_emit.up_sql_path).unwrap();
        assert!(main_up.contains("CREATE EXTENSION IF NOT EXISTS \"postgis\""));
        assert!(
            !main_up.contains("\"pg_trgm\""),
            "cross-database extension bled into main"
        );

        // Up SQL for crud_log contains CREATE EXTENSION pg_trgm but NOT postgis.
        assert!(crud_emit.up_sql_path.starts_with(&work));
        let crud_up = fs::read_to_string(&crud_emit.up_sql_path).unwrap();
        assert!(crud_up.contains("CREATE EXTENSION IF NOT EXISTS \"pg_trgm\""));
        assert!(
            !crud_up.contains("\"postgis\""),
            "cross-database extension bled into crud_log"
        );
        let temp_canon = std::env::temp_dir().canonicalize().unwrap_or_default();
        if work.starts_with(&temp_canon) {
            let _ = std::fs::remove_dir_all(&work);
        }
    }

    /// End-to-end: a model that declares
    /// `#[model(exclusion(using = "gist", elements = ["room_id WITH =",
    /// "period WITH &&"]))]` projects into a schema whose
    /// `ExclusionConstraintSchema::extension_dependency` is
    /// `Some("btree_gist")`. The emission must include
    /// `CREATE EXTENSION IF NOT EXISTS "btree_gist"`. Without this
    /// wiring adopters need to hand-write the install in a separate
    /// migration before their booking-table create can apply.
    #[test]
    fn ensure_phase_zero_aggregates_btree_gist_from_exclusion_constraints() {
        let (work, guard) = temp_workspace_with_guard("auto_extensions_excl");
        let mut models = BTreeMap::new();
        models.insert(
            BucketKey {
                database: "main".to_string(),
                app: "scheduling".to_string(),
            },
            schema_with_exclusion("bookings", "bookings_no_overlap", Some("btree_gist")),
        );
        let apps = vec![AppLifecycle {
            label: "scheduling".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];
        let emitted =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("emit");
        assert_eq!(emitted.len(), 1, "exactly one bootstrap migration (main)");
        let main_emit = emitted.iter().find(|e| e.database == "main").unwrap();
        assert_eq!(
            main_emit.extensions,
            ["btree_gist"].iter().map(|s| s.to_string()).collect(),
            "EXCLUDE-derived btree_gist must surface in bootstrap-migration extensions",
        );
        assert!(main_emit.up_sql_path.starts_with(&work));
        let up = fs::read_to_string(&main_emit.up_sql_path).unwrap();
        assert!(
            up.contains("CREATE EXTENSION IF NOT EXISTS \"btree_gist\""),
            "bootstrap-migration up SQL must auto-install btree_gist: {up}",
        );
        let temp_canon = std::env::temp_dir().canonicalize().unwrap_or_default();
        if work.starts_with(&temp_canon) {
            let _ = std::fs::remove_dir_all(&work);
        }
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
        // Pre-create all three artifacts for "main" to
        // simulate a complete prior emit. The guard now requires all
        // three to be present — a partial write (only up_path) is no
        // longer treated as complete and will be overwritten.
        let bucket = BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let dir = bucket_dir(&work, &bucket);
        assert!(dir.starts_with(&work));
        fs::create_dir_all(&dir).unwrap();
        let pend_db_dir = pending_database_dir(&work, "main");
        assert!(pend_db_dir.starts_with(&work));
        fs::create_dir_all(&pend_db_dir).unwrap();
        let up_write = dir.join(up_filename(PHASE_ZERO_VERSION));
        assert!(up_write.starts_with(&work));
        fs::write(&up_write, "-- existing bootstrap-migration up").unwrap();
        let down_write = dir.join(down_filename(PHASE_ZERO_VERSION));
        assert!(down_write.starts_with(&work));
        fs::write(&down_write, "-- existing bootstrap-migration down").unwrap();
        let legacy_pending = PendingPlan {
            format_version: PENDING_FORMAT_VERSION.to_string(),
            bucket_database: "main".to_string(),
            bucket_app: String::new(),
            version: PHASE_ZERO_VERSION.to_string(),
            slug: PHASE_ZERO_SLUG.to_string(),
            model_snapshot: empty_schema_for(&bucket),
            checksum_up: "V1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            checksum_down: None,
            composed_at: format_rfc3339_seconds(fixed_now()),
            depends_on: Vec::new(),
        };
        let legacy_write = pending_json_path(&work, &bucket);
        assert!(legacy_write.starts_with(&work));
        fs::write(
            &legacy_write,
            serde_json::to_vec_pretty(&legacy_pending).unwrap(),
        )
        .unwrap();

        let emitted =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("emit");
        assert!(
            emitted.is_empty(),
            "main was skipped (all three artifacts present); no other databases in inputs"
        );

        // Confirm the existing up-sql was NOT overwritten.
        let up_check_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        assert!(up_check_path.starts_with(&work));
        let content = fs::read_to_string(&up_check_path).unwrap();
        assert_eq!(content, "-- existing bootstrap-migration up");
        let temp_canon = std::env::temp_dir().canonicalize().unwrap_or_default();
        if work.starts_with(&temp_canon) {
            let _ = std::fs::remove_dir_all(&work);
        }
    }

    #[test]
    fn ensure_phase_zero_does_not_use_legacy_pending_fallback_when_hidden_exists() {
        let (work, guard) = temp_workspace_with_guard("auto_hidden_wins");
        let apps = vec![AppLifecycle {
            label: String::new(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];
        let models = BTreeMap::new();
        let bucket = BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let dir = bucket_dir(&work, &bucket);
        assert!(dir.starts_with(&work));
        fs::create_dir_all(&dir).unwrap();
        let up_w = dir.join(up_filename(PHASE_ZERO_VERSION));
        assert!(up_w.starts_with(&work));
        fs::write(&up_w, "-- old up").unwrap();
        let down_w = dir.join(down_filename(PHASE_ZERO_VERSION));
        assert!(down_w.starts_with(&work));
        fs::write(&down_w, "-- old down").unwrap();

        let valid_legacy_pending = PendingPlan {
            format_version: PENDING_FORMAT_VERSION.to_string(),
            bucket_database: "main".to_string(),
            bucket_app: String::new(),
            version: PHASE_ZERO_VERSION.to_string(),
            slug: PHASE_ZERO_SLUG.to_string(),
            model_snapshot: empty_schema_for(&bucket),
            checksum_up: "V1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            checksum_down: None,
            composed_at: format_rfc3339_seconds(fixed_now()),
            depends_on: Vec::new(),
        };
        let legacy_path = pending_json_path(&work, &bucket);
        assert!(legacy_path.starts_with(&work));
        ensure_parent(&legacy_path).unwrap();
        fs::write(
            &legacy_path,
            serde_json::to_vec_pretty(&valid_legacy_pending).unwrap(),
        )
        .unwrap();

        let hidden_path = phase_zero_pending_json_path(&work, "main", PHASE_ZERO_VERSION);
        let mut invalid_hidden_pending = valid_legacy_pending.clone();
        invalid_hidden_pending.version = "V00000000000001__wrong_phase_zero".to_string();
        assert!(hidden_path.starts_with(&work));
        ensure_parent(&hidden_path).unwrap();
        fs::write(
            &hidden_path,
            serde_json::to_vec_pretty(&invalid_hidden_pending).unwrap(),
        )
        .unwrap();

        let emitted =
            ensure_phase_zero_emitted(&work, &models, &apps, fixed_now(), &guard).expect("emit");
        assert_eq!(
            emitted.len(),
            1,
            "legacy pending fallback must not suppress repair when hidden pending exists"
        );
        assert_eq!(emitted[0].pending_json_path, hidden_path);

        let temp_canon = std::env::temp_dir().canonicalize().unwrap_or_default();
        if work.starts_with(&temp_canon) {
            let _ = std::fs::remove_dir_all(&work);
        }
    }
}
