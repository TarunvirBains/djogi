//! Phase 0 bootstrap — production-callable HeeRanjID + Postgres
//! extension installs.
//!
//! # Why this module exists
//!
//! `djogi migrations apply` and `djogi db reset` both need a virgin
//! Postgres database to be brought to a state where any descriptor-driven
//! migration can apply. That state is two pieces:
//!
//! 1. **HeeRanjID schema** — `generate_id()` / `generate_ranj_id()` /
//!    the `heer_*` tables / the `current_heer_node_id` GUC reader.
//!    Every model that uses `HeerId` or `RanjId` as primary key
//!    references `DEFAULT generate_id()` in its `CREATE TABLE` DDL,
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
//! `DEFAULT generate_id()`. The example papered over the gap with
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
//! - [`compose_heeranjid_install`] — owned SQL string for the
//!   HeeRanjID schema + desc support + seed.
//! - [`compose_extension_installs`] — owned SQL string for
//!   `CREATE EXTENSION IF NOT EXISTS <name>` over a sorted set.
//! - [`compose_node_seed`] — owned SQL string for the
//!   `ALTER DATABASE ... SET` + session-level `SET` of
//!   `heer.node_id`.
//! - [`compose_phase_zero`] — combined SQL: HeeRanjID install +
//!   extensions + node seed. The auto-emit path writes this to disk;
//!   the test harness runs it directly.
//! - [`run_phase_zero`] — runtime driver that executes the composed
//!   SQL via a `tokio_postgres::GenericClient`. Routes through the
//!   per-database client the caller supplies.
//! - [`extension_dependencies_from_models`] — collects the distinct
//!   `extension_dependency` values across a per-bucket `AppliedSchema`
//!   map. Used by `migrations compose` to build the `extensions`
//!   argument when auto-emitting Phase 0.
//! - [`BootstrapError`] — error variants surfaced by the runtime
//!   driver and the validator.

use std::collections::{BTreeMap, BTreeSet};

use tokio_postgres::GenericClient;

use super::projection::BucketKey;
use super::schema::AppliedSchema;

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
            BootstrapError::InvalidExtensionName { .. } => None,
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
pub fn compose_heeranjid_install() -> String {
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
pub fn compose_extension_installs(extensions: &BTreeSet<String>) -> Result<String, BootstrapError> {
    // Validate up-front — any failure surfaces before the output
    // String is allocated. This keeps the function output partition
    // clean: either every extension was acceptable and SQL is
    // returned, or no SQL is returned and the operator gets a
    // structured error naming the bad input.
    for name in extensions {
        validate_extension_name(name)?;
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
pub fn compose_node_seed(database: &str, node_id: i32) -> Result<String, BootstrapError> {
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
pub fn compose_phase_zero(
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
/// map.
///
/// Used by `migrations compose` to build the `extensions` argument
/// passed to [`compose_phase_zero`] when auto-emitting Phase 0.
///
/// Walks every index on every bucket. `AppliedSchema::indexes` is the
/// flat per-bucket index list — each entry already carries its
/// `table` field so the per-bucket level is the correct walk.
/// `None`-valued dependencies (the common case for stock BTree / GIN
/// indexes) are skipped; named dependencies (e.g. `"postgis"`,
/// `"pg_trgm"`) are inserted into the result set. Duplicates collapse
/// — declaring a PostGIS-dependent index on five models still produces
/// one `CREATE EXTENSION IF NOT EXISTS "postgis"`.
///
/// Returns owned strings keyed by `BTreeSet<String>` so the result
/// is sorted + de-duplicated and the caller can hash it deterministically.
pub fn extension_dependencies_from_models(
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
}
