//! Live-database verification — compares the on-disk snapshot, the
//! `djogi_schema_migrations` ledger, and the live Postgres catalog,
//! producing a deterministic list of [`VerifyDiagnostic`] entries.
//!
//! # Scope (Phase 7 v3 §8 / v2 T5)
//!
//! Verify answers three questions:
//!
//! 1. **Ledger ↔ catalog.** Every migration the ledger says is
//!    `applied` should show up in the live catalog (its tables,
//!    columns, indexes, foreign keys exist).
//! 2. **Snapshot ↔ catalog.** The current `schema_snapshot.json` for
//!    the bucket should match the live catalog. Any drift surfaces as
//!    a `D6xx` diagnostic.
//! 3. **Snapshot ↔ ledger.** The most recent applied ledger row
//!    should carry a checksum that re-validates against the snapshot's
//!    declared format version. Format errors surface as `D6xx`.
//!
//! Verify never mutates anything — it is strictly read-only against
//! the live database and the snapshot file. The previous T5 entry
//! point bootstrapped the ledger table on the way in, which violated
//! the "verify never writes" contract on a fresh DB; that has been
//! removed and a missing ledger now surfaces as a typed `D621`
//! Error diagnostic instead. Mutations belong to [`super::repair`].
//!
//! # Minimum viable verify (T5)
//!
//! T5 reads the live catalog into a *partial* [`AppliedSchema`]
//! containing only what the verify path needs to compare:
//!
//! - **Tables.** Name + column list (name, rendered SQL type,
//!   nullability, default expression).
//! - **Primary keys.** Column list — diffed (B-6). Kind detection
//!   stays deferred to T8.
//! - **Indexes.** Name + table + columns (in order) + uniqueness +
//!   method — diffed (B-7). `INCLUDE` and partial-predicate surface
//!   as `Info` (T5 stop condition).
//! - **Foreign keys.** Source `(table, column)` + target
//!   `(table, column)` + cascade + deferrability — diffed as D609.
//!
//! Other fields ([`crate::migrate::schema::TableSchema::fts`],
//! [`crate::migrate::schema::TableSchema::partition`],
//! [`crate::migrate::schema::TableSchema::tenant_key`], enum types)
//! surface as advisory `Info` diagnostics for Phase 7 — T8 can
//! tighten them to `Error` once the live-DB projection grows. The
//! deferral is intentional: the v3 plan's stop condition explicitly
//! says ">500 LOC of catalog SQL is a sign you should narrow scope
//! and surface it for review". Any tightening lands in T8 alongside
//! the `migrations status` work.
//!
//! # Diagnostic codes (D6xx range)
//!
//! Verify's diagnostic codes live in the `D6xx` namespace (D025 is
//! T4's guard, D004 is build-rs folder drift). Each code has a stable
//! meaning — re-using a code for a different condition is a hard
//! reviewer ding. Current assignments:
//!
//! | Code | Severity | Meaning |
//! |------|----------|---------|
//! | D601 | Error    | Snapshot table missing from live DB. |
//! | D602 | Error    | Live table not present in snapshot. |
//! | D603 | Error    | Snapshot column missing from live DB. |
//! | D604 | Error    | Live column not present in snapshot. |
//! | D605 | Error    | Nullability drift between snapshot and live. |
//! | D606 | Warning  | SQL-type rendering drift (advisory). |
//! | D607 | Error    | Column DEFAULT differs between snapshot and live. |
//! | D608 | Error    | Primary key column list differs. |
//! | D609 | Error    | Foreign-key shape differs. |
//! | D610 | Error    | Snapshot index missing from live DB. |
//! | D611 | Warning  | Live index not present in snapshot. |
//! | D612 | Error    | Index columns differ (shape mismatch). |
//! | D613 | Error    | Index uniqueness differs. |
//! | D614 | Warning  | Index method (btree / gin / ...) differs. |
//! | D615 | Error    | Index is on the wrong table. |
//! | D621 | Error    | Ledger table is missing — run apply / baseline first. |
//! | D622 | Warning  | Out-of-order migration applied — `out_of_order_flag = TRUE`. Strict mode upgrades to Error. |
//! | D690 | Info     | FTS configuration declared but not yet checked. |
//! | D691 | Info     | Partition strategy declared but not yet checked. |
//! | D692 | Info     | Enum types declared but not yet checked. |
//! | D693 | Info     | Index `INCLUDE` columns / predicate not yet checked. |
//! | D699 | Error    | Ledger reports applied rows but DB has no tables. |
//!
//! Every `D6xx` code is unique. Adding a new code goes at the end of
//! whichever sub-range matches the topic (60x for table, 61x for index,
//! 62x for ledger lifecycle, 69x for advisory).
//!
//! # Determinism
//!
//! Output ordering is stable. [`VerifyDiagnostic`] lists are sorted
//! by `(code, location)` before return, and every catalog query that
//! powers the projection uses an explicit `ORDER BY` clause so the
//! comparison surface is reproducible. No `HashMap` / `HashSet` in
//! the public path.
//!
//! # HeeRanjID artifact tables
//!
//! Some Postgres tables belong to the HeeRanjID substrate and must
//! never surface as drift even when an adopter declares a
//! legitimately-named table starting with `heer_`. To avoid the
//! prefix-exclusion footgun the verify projection uses an explicit
//! sorted allowlist of HeeRanjID table names — see
//! [`HEERANJID_ARTIFACT_TABLES`]. Adding a new HeeRanjID table goes
//! through the HeeRanjID workspace; Djogi only mirrors the names.
//!
//! # Postgres-only
//!
//! Per the Djogi-wide Postgres-18-only stance, queries reach into
//! `pg_class`, `pg_attribute`, `pg_index`, `pg_constraint`,
//! `pg_attrdef`, and `information_schema.columns`. The selection
//! preserves the ability to read materialised columns Postgres does
//! not surface through `information_schema` (e.g. `pg_attribute.atttypmod`
//! for `VARCHAR(N)` length).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::context::DjogiContext;
use crate::error::DjogiError;

use super::ledger::{LEDGER_SELECT_COLS, LedgerRow, LedgerStatus};
use super::projection::BucketKey;
use super::schema::{
    AppliedSchema, ColumnSchema, ForeignKeySchema, IndexColumnSchema, IndexKindSchema,
    IndexNullsOrderSchema, IndexOrderSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
    OnDeleteSchema, PrimaryKeySchema, TableSchema,
};

/// Sorted allowlist of HeeRanjID artifact table names. Verify
/// excludes these from the live-DB projection so HeeRanjID's
/// substrate tables do not show up as "extra live tables" during
/// drift checks.
///
/// **Adopter-owned tables that legitimately start with `heer_` are
/// preserved.** The previous arrangement used `NOT LIKE 'heer\\_%'`
/// which silently dropped any adopter-owned table whose name began
/// with `heer_`. The allowlist is the precise fix (A-1): only
/// HeeRanjID's own tables are excluded.
///
/// Source of truth: HeeRanjID's `sql/postgres/schema.sql`. Update
/// this list whenever HeeRanjID adds or removes a substrate table.
/// Sorted alphabetically so binary_search works.
pub(crate) const HEERANJID_ARTIFACT_TABLES: &[&str] = &[
    "heer_config",
    "heer_node_state",
    "heer_nodes",
    "heer_ranj_node_state",
];

/// `true` when `name` is a HeeRanjID substrate table that verify
/// must exclude from drift comparisons. Uses `binary_search` against
/// the sorted [`HEERANJID_ARTIFACT_TABLES`] allowlist.
pub(crate) fn is_heeranjid_artifact_table(name: &str) -> bool {
    HEERANJID_ARTIFACT_TABLES.binary_search(&name).is_ok()
}

// ── Public output shapes ──────────────────────────────────────────────────

/// Severity of a [`VerifyDiagnostic`]. Verify exits non-zero on any
/// `Error`; `Warning` and `Info` are advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerifySeverity {
    /// Informational — surfacing a known limitation of the verify
    /// projection (e.g. "FTS schema is not yet checked"). Verify
    /// returns success.
    Info,
    /// Warning — the verify projection found something off, but the
    /// operator may legitimately have chosen the configuration. Verify
    /// returns success.
    Warning,
    /// Error — a hard mismatch between snapshot, ledger, or live DB
    /// that the operator should resolve. Verify returns non-zero.
    Error,
}

/// One verify diagnostic. The `code` follows Djogi's `D###`
/// convention (D6xx is verify's reserved range — D025 lives in T4's
/// guard, D004 in build-rs folder drift).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyDiagnostic {
    /// Stable identifier (e.g. `"D601"`). Operators reference this in
    /// follow-ups and the doc index pins each code to its meaning.
    pub code: String,
    /// Severity — drives the verify exit code.
    pub severity: VerifySeverity,
    /// One-line operator-facing message. Should name the offending
    /// object (table / column / index) so the operator can find it
    /// without reading code.
    pub message: String,
    /// Optional location string (e.g. `"users.email"`,
    /// `"index:users_email_idx"`). Used by the deterministic sort key
    /// alongside `code`.
    pub location: Option<String>,
}

impl VerifyDiagnostic {
    /// Sort key — `(code, location)`. The pair is stable across runs
    /// because both fields come from owned strings derived from the
    /// catalog or snapshot, never from a randomised hash.
    fn sort_key(&self) -> (String, String) {
        (self.code.clone(), self.location.clone().unwrap_or_default())
    }
}

/// Result of a verify run.
///
/// `diagnostics` is sorted by `(code, location)` for determinism.
/// `has_errors()` returns true when at least one diagnostic carries
/// [`VerifySeverity::Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// All diagnostics produced this run, sorted alphabetically by
    /// `(code, location)`.
    pub diagnostics: Vec<VerifyDiagnostic>,
    /// The most recent applied ledger row, if any. `None` when the
    /// ledger is empty (fresh database).
    pub latest_applied_version: Option<String>,
    /// Number of `applied` ledger rows seen.
    pub applied_count: usize,
    /// Number of `failed` / `pending` ledger rows seen.
    pub unfinished_count: usize,
}

impl VerifyReport {
    /// Returns `true` when the report contains at least one
    /// [`VerifySeverity::Error`] diagnostic.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == VerifySeverity::Error)
    }

    /// Returns `true` when the report contains at least one
    /// [`VerifySeverity::Warning`] diagnostic.
    pub fn has_warnings(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == VerifySeverity::Warning)
    }
}

/// Errors surfaced while running verify itself (distinct from
/// "verify ran fine and reports D6xx errors"). Failure to read the
/// snapshot or the catalog short-circuits before any diagnostic is
/// emitted.
#[derive(Debug)]
pub enum VerifyRunError {
    /// Loading the snapshot file failed.
    SnapshotLoadFailed {
        path: PathBuf,
        source: super::snapshot::SnapshotError,
    },
    /// Reading the live catalog failed.
    CatalogQueryFailed {
        query_label: &'static str,
        source: DjogiError,
    },
    /// Reading the ledger failed.
    LedgerQueryFailed { source: DjogiError },
}

impl std::fmt::Display for VerifyRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyRunError::SnapshotLoadFailed { path, source } => {
                write!(
                    f,
                    "verify could not load snapshot at {}: {source}",
                    path.display()
                )
            }
            VerifyRunError::CatalogQueryFailed {
                query_label,
                source,
            } => write!(f, "verify catalog query `{query_label}` failed: {source}"),
            VerifyRunError::LedgerQueryFailed { source } => {
                write!(f, "verify ledger read failed: {source}")
            }
        }
    }
}

impl std::error::Error for VerifyRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VerifyRunError::SnapshotLoadFailed { source, .. } => Some(source),
            VerifyRunError::CatalogQueryFailed { source, .. } => Some(source),
            VerifyRunError::LedgerQueryFailed { source } => Some(source),
        }
    }
}

// ── Public entry points ───────────────────────────────────────────────────

/// Run verify against the live database, comparing the supplied
/// snapshot to the live catalog and the ledger.
///
/// **Read-only (B-8).** Verify never writes. The previous T5 arrangement
/// called `ledger::bootstrap` on the way in, which created
/// `djogi_schema_migrations` on a fresh DB — a hard violation of the
/// "verify never mutates" contract. The fix: verify probes for the
/// ledger via a `pg_class` lookup, and a missing ledger surfaces as a
/// typed `D621` Error diagnostic ("ledger table not found — run
/// `djogi migrations apply` or `djogi migrations baseline` first").
/// Verify returns successfully (with the diagnostic) instead of
/// mutating state.
///
/// **Determinism.** `diagnostics` is sorted by `(code, location)`.
/// Iteration over the live catalog uses ordered queries so a re-run
/// against an unchanged DB produces an identical report.
pub async fn verify(
    ctx: &mut DjogiContext,
    snapshot: &AppliedSchema,
) -> Result<VerifyReport, VerifyRunError> {
    // Default policy: lenient on out-of-order. Callers that want
    // stricter D622 severity invoke [`verify_with_policy`] directly
    // with `strict_out_of_order = true`.
    verify_with_policy(ctx, snapshot, &crate::config::PolicyConfig::default()).await
}

/// Same as [`verify`] but threads a [`crate::config::PolicyConfig`]
/// so the caller can pin the severity of T7's `D622` (out-of-order
/// applied migration) diagnostic.
///
/// **Why a separate entry point.** The historical signature
/// `verify(ctx, snapshot)` is consumed by every existing caller. Adding
/// a `policy: &PolicyConfig` parameter would churn every call site even
/// though most callers use the default lenient policy. A second entry
/// point keeps the existing API and gives strict-mode callers the new
/// surface they need.
pub async fn verify_with_policy(
    ctx: &mut DjogiContext,
    snapshot: &AppliedSchema,
    policy: &crate::config::PolicyConfig,
) -> Result<VerifyReport, VerifyRunError> {
    let mut diagnostics: Vec<VerifyDiagnostic> = Vec::new();

    // B-8: probe for the ledger without bootstrapping it. Verify is
    // read-only; on a fresh DB we surface D621 and leave the ledger
    // un-created. The probe uses pg_class so the SELECT below can
    // never fail with relation-not-found.
    let ledger_present = ledger_table_exists(ctx).await?;

    let (applied_count, unfinished_count, latest_applied_version) = if ledger_present {
        let ledger_rows = read_applied_ledger(ctx).await?;
        let applied_count = ledger_rows
            .iter()
            .filter(|r| r.status == LedgerStatus::Applied)
            .count();
        let unfinished_count = ledger_rows
            .iter()
            .filter(|r| matches!(r.status, LedgerStatus::Pending | LedgerStatus::Failed))
            .count();
        let latest_applied_version = ledger_rows
            .iter()
            .rev()
            .find(|r| r.status == LedgerStatus::Applied)
            .map(|r| r.version.clone());

        // D699: ledger reports applied migrations but the live DB has
        // zero user tables — the schema was likely dropped out-of-band.
        // (This used to share `D610` with the index-missing diagnostic;
        // A-2 split them so each code carries one stable meaning.)
        let live_for_ledger_check = project_live_schema(ctx).await?;
        if !ledger_rows.is_empty()
            && live_for_ledger_check.models.is_empty()
            && !snapshot.models.is_empty()
        {
            diagnostics.push(VerifyDiagnostic {
                code: "D699".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "ledger reports {applied_count} applied migration(s) but the \
                     live database contains zero tables; the schema may have been \
                     dropped out-of-band",
                ),
                location: None,
            });
        }

        // T7 / D622: surface every ledger row whose `out_of_order_flag`
        // is set. Each occurrence becomes its own diagnostic entry so
        // the operator can see the full list. Severity is Warning by
        // default; `policy.strict_out_of_order = true` upgrades to
        // Error so verify exits non-zero on any historical drift.
        emit_out_of_order_diagnostics(&ledger_rows, policy, &mut diagnostics);

        (applied_count, unfinished_count, latest_applied_version)
    } else {
        diagnostics.push(VerifyDiagnostic {
            code: "D621".to_string(),
            severity: VerifySeverity::Error,
            message: "ledger table `djogi_schema_migrations` not found — run \
                      `djogi migrations apply` or `djogi migrations baseline` \
                      first; verify is read-only and will not bootstrap the ledger"
                .to_string(),
            location: None,
        });
        (0, 0, None)
    };

    // Project the live catalog. Any catalog read failure is fatal —
    // we cannot produce useful diagnostics from a partial read.
    let live = project_live_schema(ctx).await?;

    // Compare snapshot tables to live tables (includes per-column
    // default + nullability + type comparison; PK comparison from B-6).
    diff_tables(snapshot, &live, &mut diagnostics);

    // Compare snapshot indexes to live indexes — name + table +
    // columns + uniqueness + method (B-7). INCLUDE / partial
    // predicate surface as Info per the T5 stop condition.
    diff_indexes(snapshot, &live, &mut diagnostics);

    // Surface advisory diagnostics for fields the projection does not
    // yet exercise so operators know the limit of T5's verify scope.
    diff_advisory_fields(snapshot, &mut diagnostics);

    diagnostics.sort_by_key(|d| d.sort_key());

    Ok(VerifyReport {
        diagnostics,
        latest_applied_version,
        applied_count,
        unfinished_count,
    })
}

/// Walk the ledger rows and emit a D622 diagnostic for each row whose
/// `out_of_order_flag` is set. Severity follows
/// [`crate::config::PolicyConfig::strict_out_of_order`] — Warning when
/// false, Error when true.
///
/// The conflicting peer's identity is recovered from the ledger by
/// finding the highest-version row that was already applied at the
/// time. We do that here rather than at apply time so verify can
/// re-explain history without depending on a transient runtime
/// signal.
fn emit_out_of_order_diagnostics(
    ledger_rows: &[LedgerRow],
    policy: &crate::config::PolicyConfig,
    diagnostics: &mut Vec<VerifyDiagnostic>,
) {
    let severity = if policy.strict_out_of_order {
        VerifySeverity::Error
    } else {
        VerifySeverity::Warning
    };
    for row in ledger_rows {
        if !row.out_of_order_flag {
            continue;
        }
        // Find the lexically-greater peer: the highest version among
        // applied / faked / baseline rows in the same app_label that
        // sorts above our row. This is the conflict surface.
        let conflicting_peer: Option<&LedgerRow> = ledger_rows
            .iter()
            .filter(|r| {
                r.app_label == row.app_label
                    && r.version > row.version
                    && matches!(
                        r.status,
                        LedgerStatus::Applied | LedgerStatus::Faked | LedgerStatus::Baseline
                    )
            })
            .max_by(|a, b| a.version.cmp(&b.version));
        let location = format!("version:{}", row.version);
        let message = match conflicting_peer {
            Some(peer) => format!(
                "out-of-order migration `{version}` (app={app}) was applied \
                 below peer `{peer}` (which carries the same or higher \
                 version)",
                version = row.version,
                app = if row.app_label.is_empty() {
                    "_global_"
                } else {
                    row.app_label.as_str()
                },
                peer = peer.version,
            ),
            None => format!(
                "out-of-order migration `{version}` (app={app}) was \
                 recorded with out_of_order_flag = TRUE but no \
                 higher-version peer is currently in the ledger; the \
                 peer may have been rolled back",
                version = row.version,
                app = if row.app_label.is_empty() {
                    "_global_"
                } else {
                    row.app_label.as_str()
                },
            ),
        };
        diagnostics.push(VerifyDiagnostic {
            code: "D622".to_string(),
            severity,
            message,
            location: Some(location),
        });
    }
}

/// Returns `true` when `djogi_schema_migrations` exists in the
/// `public` schema. Used by verify to gate the ledger read without
/// bootstrapping (B-8).
async fn ledger_table_exists(ctx: &mut DjogiContext) -> Result<bool, VerifyRunError> {
    let row = ctx
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = 'public' \
                   AND c.relname = 'djogi_schema_migrations' \
                   AND c.relkind = 'r' \
             )",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "ledger_present_probe",
            source: e,
        })?;
    let exists: bool = row
        .try_get(0)
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "ledger_present_probe.bool",
            source: DjogiError::from(e),
        })?;
    Ok(exists)
}

// ── Live-DB projection (read-only) ────────────────────────────────────────

/// Read enough of the live catalog to compare against an
/// [`AppliedSchema`]. The projection captures tables, columns,
/// primary keys, indexes, and foreign keys — see the module docs for
/// the scope rationale.
///
/// **Excludes the migration ledger table itself.** The
/// `djogi_schema_migrations` table is bookkeeping, not user schema —
/// surfacing it as drift would noise up every verify run.
async fn project_live_schema(ctx: &mut DjogiContext) -> Result<AppliedSchema, VerifyRunError> {
    let foreign_keys = read_foreign_keys(ctx).await?;
    let tables = read_tables(ctx, &foreign_keys).await?;
    let mut models: BTreeMap<String, TableSchema> = BTreeMap::new();
    for t in tables {
        models.insert(t.table.clone(), t);
    }

    let indexes = read_indexes(ctx).await?;

    Ok(AppliedSchema {
        djogi_version: String::new(),
        enums: BTreeMap::new(),
        format_version: super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: String::new(),
        indexes,
        models,
        registered_apps: Vec::new(),
    })
}

/// Read every user table in the `public` schema along with its
/// columns. Excludes framework-internal bookkeeping tables:
///
/// - `djogi_schema_migrations` — the ledger.
/// - HeeRanjID artifact tables — see [`HEERANJID_ARTIFACT_TABLES`].
///   Adopter-owned tables that legitimately start with `heer_` are
///   preserved (A-1 fix).
async fn read_tables(
    ctx: &mut DjogiContext,
    foreign_keys: &[LiveForeignKey],
) -> Result<Vec<TableSchema>, VerifyRunError> {
    // Step 1 — table names. Postgres 18 only; we rely on
    // `pg_class.relkind = 'r'` for ordinary tables and filter out
    // the ledger here. HeeRanjID artifact tables are filtered in
    // Rust against the [`HEERANJID_ARTIFACT_TABLES`] allowlist after
    // the query (A-1) — the previous LIKE 'heer\\_%' silently
    // dropped adopter-owned tables that legitimately start with
    // `heer_`.
    let table_rows = ctx
        .query_all(
            "SELECT c.relname::text \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r' \
               AND n.nspname = 'public' \
               AND c.relname <> 'djogi_schema_migrations' \
             ORDER BY c.relname",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "tables",
            source: e,
        })?;

    // Batch the per-table catalog reads: collapses N round-trips for
    // columns + N round-trips for primary keys into two queries
    // total (cluster-2 simplify Finding 4). The batched helpers scan
    // the whole `public` schema; we look up each table by name in
    // the loop below.
    let mut columns_by_table = read_all_columns(ctx).await?;
    let mut pk_by_table = read_all_primary_key_columns(ctx).await?;

    let mut out: Vec<TableSchema> = Vec::with_capacity(table_rows.len());
    for row in &table_rows {
        let table_name: String =
            row.try_get(0)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "tables.relname",
                    source: DjogiError::from(e),
                })?;
        if is_heeranjid_artifact_table(&table_name) {
            continue;
        }
        let mut columns = columns_by_table.remove(&table_name).unwrap_or_default();
        attach_foreign_keys(&table_name, &mut columns, foreign_keys);
        let primary_key_columns = pk_by_table.remove(&table_name).unwrap_or_default();

        out.push(TableSchema {
            app: None,
            columns,
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: super::schema::PrimaryKeySchema {
                columns: primary_key_columns,
                // Live PG cannot tell us the PK kind without reaching
                // into the column DEFAULT expression; T5's verify keeps
                // the kind comparison in advisory mode (`HeerId` is the
                // common case but the projection cannot prove it from
                // the catalog alone). T8 tightens this.
                kind: super::schema::PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: table_name,
            tenant_key: None,
            // Verify reads the live catalog only — exclusion constraint
            // discovery via `pg_constraint.contype = 'x'` is a future
            // verify pass (mirrors how indexes/FKs are read). Empty here
            // means "verify did not assert anything about the table's
            // exclusion constraints," not "the table has none."
            exclusion_constraints: Vec::new(),
        });
    }
    Ok(out)
}

/// Read column metadata for every public-schema ordinary table in a
/// single catalog query (cluster-2 simplify Finding 4). Returns a
/// map keyed by table name, where each value is the columns in
/// `pg_attribute.attnum` order. Replaces the previous per-table
/// helper that ran one round-trip per table; verify now does one
/// `pg_attribute` scan total regardless of table count.
async fn read_all_columns(
    ctx: &mut DjogiContext,
) -> Result<BTreeMap<String, Vec<ColumnSchema>>, VerifyRunError> {
    // Same projection as the per-table query, plus `c.relname` so we
    // can group rows by table in Rust. `c.relkind = 'r'` keeps the
    // scan to ordinary tables (matching the per-table caller's
    // implicit shape).
    let rows = ctx
        .query_all(
            "SELECT c.relname::text, \
                    a.attname::text, \
                    format_type(a.atttypid, a.atttypmod)::text, \
                    a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE n.nspname = 'public' \
               AND c.relkind = 'r' \
               AND a.attnum > 0 \
               AND NOT a.attisdropped \
             ORDER BY c.relname, a.attnum",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "columns",
            source: e,
        })?;

    let mut out: BTreeMap<String, Vec<ColumnSchema>> = BTreeMap::new();
    for row in &rows {
        let table_name: String =
            row.try_get(0)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "columns.relname",
                    source: DjogiError::from(e),
                })?;
        let name: String = row
            .try_get(1)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "columns.attname",
                source: DjogiError::from(e),
            })?;
        let sql_type: String = row
            .try_get(2)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "columns.type",
                source: DjogiError::from(e),
            })?;
        let attnotnull: bool = row
            .try_get(3)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "columns.attnotnull",
                source: DjogiError::from(e),
            })?;
        let default_sql: Option<String> =
            row.try_get(4)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "columns.default",
                    source: DjogiError::from(e),
                })?;

        out.entry(table_name).or_default().push(ColumnSchema {
            check: None,
            default_sql,
            foreign_key: None,
            // Verify does not yet read `pg_attrdef` to discover stored
            // generated expressions; advisory mode treats every live
            // column as non-generated. T8/future verify pass tightens.
            generated: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name,
            // `pg_attribute.attnotnull == true` means NOT NULL ⇒
            // nullable = false.
            nullable: !attnotnull,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: render_type_for_compare(&sql_type),
            unique: false,
        });
    }
    Ok(out)
}

fn attach_foreign_keys(
    table_name: &str,
    columns: &mut [ColumnSchema],
    foreign_keys: &[LiveForeignKey],
) {
    for column in columns {
        if let Some(fk) = foreign_keys
            .iter()
            .find(|fk| fk.table == table_name && fk.column == column.name)
        {
            column.foreign_key = Some(ForeignKeySchema {
                deferrable: fk.deferrable,
                initially_deferred: fk.initially_deferred,
                on_delete: fk.on_delete,
                ref_column: fk.ref_column.clone(),
                ref_table: fk.ref_table.clone(),
            });
            column.on_delete = Some(fk.on_delete);
        }
    }
}

/// Read the primary-key column list for every public-schema table
/// in a single catalog query (cluster-2 simplify Finding 4). Returns
/// a map keyed by table name; each value is the PK columns in
/// declaration order. Replaces the per-table helper that ran one
/// round-trip per table.
async fn read_all_primary_key_columns(
    ctx: &mut DjogiContext,
) -> Result<BTreeMap<String, Vec<String>>, VerifyRunError> {
    let rows = ctx
        .query_all(
            "SELECT c.relname::text, \
                    a.attname::text \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = con.conrelid \
                                 AND a.attnum = ANY(con.conkey) \
             WHERE n.nspname = 'public' \
               AND con.contype = 'p' \
             ORDER BY c.relname, array_position(con.conkey, a.attnum)",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "primary_key",
            source: e,
        })?;

    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        let table_name: String =
            row.try_get(0)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "primary_key.relname",
                    source: DjogiError::from(e),
                })?;
        let col: String = row
            .try_get(1)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "primary_key.attname",
                source: DjogiError::from(e),
            })?;
        out.entry(table_name).or_default().push(col);
    }
    Ok(out)
}

/// Read every non-PK index in `public` along with its shape (B-7) —
/// columns in order, uniqueness, method (btree / gin / ...).
///
/// Skips:
///
/// - PK indexes (the column list lives on the table's
///   [`super::schema::PrimaryKeySchema`]).
/// - HeeRanjID artifact tables — see [`HEERANJID_ARTIFACT_TABLES`].
/// - The ledger table.
///
/// `INCLUDE(...)` columns and partial-predicate `WHERE` clauses are
/// deliberately NOT projected here — the T5 stop condition keeps
/// those at advisory `Info` level for now (D693). T8 will tighten.
async fn read_indexes(ctx: &mut DjogiContext) -> Result<Vec<IndexSchema>, VerifyRunError> {
    // Step 1 — one row per index with name + table + uniqueness +
    // access method. Step 2 (read_all_index_columns, batched per
    // cluster-2 simplify Finding 4) produces every index's per-
    // column list in one query so the per-index loop below is a
    // pure in-memory join.
    let rows = ctx
        .query_all(
            "SELECT i.relname::text, \
                    t.relname::text, \
                    ix.indisunique, \
                    am.amname::text \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_class t ON t.oid = ix.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_am am ON am.oid = i.relam \
             WHERE n.nspname = 'public' \
               AND ix.indisprimary = false \
               AND t.relname <> 'djogi_schema_migrations' \
             ORDER BY i.relname",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "indexes",
            source: e,
        })?;

    // Batch the per-index column-list reads (cluster-2 simplify
    // Finding 4): collapses N round-trips into one `pg_index` scan.
    let mut columns_by_index = read_all_index_columns(ctx).await?;

    let mut out: Vec<IndexSchema> = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row
            .try_get(0)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "indexes.relname",
                source: DjogiError::from(e),
            })?;
        let table: String = row
            .try_get(1)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "indexes.table",
                source: DjogiError::from(e),
            })?;
        if is_heeranjid_artifact_table(&table) {
            continue;
        }
        let is_unique: bool = row
            .try_get(2)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "indexes.indisunique",
                source: DjogiError::from(e),
            })?;
        let amname: String = row
            .try_get(3)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "indexes.amname",
                source: DjogiError::from(e),
            })?;

        let index_columns = columns_by_index.remove(&name).unwrap_or_default();

        out.push(IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: pg_amname_to_index_type(&amname),
            kind: if is_unique {
                IndexKindSchema::UniqueIndex
            } else {
                IndexKindSchema::NonUnique
            },
            name,
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table,
            target: IndexTargetSchema::Columns(index_columns),
        });
    }
    Ok(out)
}

/// Read the per-column list for every public-schema index in a
/// single catalog query (cluster-2 simplify Finding 4). Returns a
/// map keyed by index name; each value is the columns in
/// `pg_index.indkey` order. Each [`IndexColumnSchema`] carries the
/// default sort direction / nulls policy / opclass — the live
/// catalog read stops short of opclass detection (T8 territory) so
/// the snapshot's own knobs are the comparison ground truth.
/// Replaces the per-index helper that ran one round-trip per index.
async fn read_all_index_columns(
    ctx: &mut DjogiContext,
) -> Result<BTreeMap<String, Vec<IndexColumnSchema>>, VerifyRunError> {
    let rows = ctx
        .query_all(
            "SELECT i.relname::text, \
                    a.attname::text \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_namespace n ON n.oid = i.relnamespace \
             JOIN unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON TRUE \
             JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = k.attnum \
             WHERE n.nspname = 'public' \
               AND a.attnum > 0 \
             ORDER BY i.relname, k.ord",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "index_columns",
            source: e,
        })?;
    let mut out: BTreeMap<String, Vec<IndexColumnSchema>> = BTreeMap::new();
    for row in &rows {
        let index_name: String =
            row.try_get(0)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "index_columns.relname",
                    source: DjogiError::from(e),
                })?;
        let col_name: String = row
            .try_get(1)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "index_columns.attname",
                source: DjogiError::from(e),
            })?;
        out.entry(index_name).or_default().push(IndexColumnSchema {
            name: col_name,
            nulls: IndexNullsOrderSchema::Default,
            opclass: None,
            order: IndexOrderSchema::Asc,
        });
    }
    Ok(out)
}

/// Map a Postgres `pg_am.amname` string to the corresponding
/// [`IndexTypeSchema`] variant. Unknown methods fall back to
/// `BTree`; T8 will surface that as a warning, but T5 keeps the
/// projection forgiving so the diff path can still proceed.
fn pg_amname_to_index_type(amname: &str) -> IndexTypeSchema {
    match amname {
        "btree" => IndexTypeSchema::BTree,
        "hash" => IndexTypeSchema::Hash,
        "gin" => IndexTypeSchema::Gin,
        "gist" => IndexTypeSchema::Gist,
        "spgist" => IndexTypeSchema::Spgist,
        "brin" => IndexTypeSchema::Brin,
        _ => IndexTypeSchema::BTree,
    }
}

/// Read the ledger rows we use for verification. Returns rows in
/// `applied_at` order so iteration is chronological.
///
/// **Caller must confirm the ledger exists (B-8).** Verify probes for
/// the ledger via [`ledger_table_exists`] before calling this helper;
/// running it without that check would surface a relation-not-found
/// from the SELECT below. The verify entry point handles the missing
/// ledger by emitting `D621` and skipping this read entirely.
///
/// Uses [`LedgerRow::try_from`] (cluster-2 simplify Finding 3) to
/// centralize the 14-column try_get cascade in ledger.rs.
async fn read_applied_ledger(ctx: &mut DjogiContext) -> Result<Vec<LedgerRow>, VerifyRunError> {
    let sql = format!("{LEDGER_SELECT_COLS} ORDER BY applied_at, version");
    let rows = ctx
        .query_all(&sql, &[])
        .await
        .map_err(|e| VerifyRunError::LedgerQueryFailed { source: e })?;

    let mut out: Vec<LedgerRow> = Vec::with_capacity(rows.len());
    for row in &rows {
        let ledger_row =
            LedgerRow::try_from(row).map_err(|e| VerifyRunError::LedgerQueryFailed {
                source: DjogiError::from(e),
            })?;
        out.push(ledger_row);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveForeignKey {
    table: String,
    column: String,
    ref_table: String,
    ref_column: String,
    on_delete: OnDeleteSchema,
    deferrable: bool,
    initially_deferred: bool,
}

/// Read every foreign-key constraint in `public`, including the
/// deferrability bits needed to detect D609 drift.
async fn read_foreign_keys(ctx: &mut DjogiContext) -> Result<Vec<LiveForeignKey>, VerifyRunError> {
    let rows = ctx
        .query_all(
            "SELECT c.relname::text, \
                    a.attname::text, \
                    rc.relname::text, \
                    ra.attname::text, \
                    con.confdeltype::text, \
                    con.condeferrable, \
                    con.condeferred \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = con.conrelid \
                                 AND a.attnum = ANY(con.conkey) \
             JOIN pg_class rc ON rc.oid = con.confrelid \
             JOIN pg_attribute ra ON ra.attrelid = con.confrelid \
                                  AND ra.attnum = ANY(con.confkey) \
             WHERE n.nspname = 'public' \
               AND con.contype = 'f' \
               AND array_position(con.conkey, a.attnum) \
                   = array_position(con.confkey, ra.attnum) \
             ORDER BY c.relname, a.attname",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "foreign_keys",
            source: e,
        })?;

    let mut out: Vec<LiveForeignKey> = Vec::with_capacity(rows.len());
    for row in &rows {
        let table: String = row
            .try_get(0)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "foreign_keys.table",
                source: DjogiError::from(e),
            })?;
        let column: String = row
            .try_get(1)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "foreign_keys.column",
                source: DjogiError::from(e),
            })?;
        let ref_table: String = row
            .try_get(2)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "foreign_keys.ref_table",
                source: DjogiError::from(e),
            })?;
        let ref_column: String =
            row.try_get(3)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "foreign_keys.ref_column",
                    source: DjogiError::from(e),
                })?;
        let on_delete_code: String =
            row.try_get(4)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "foreign_keys.confdeltype",
                    source: DjogiError::from(e),
                })?;
        let deferrable: bool = row
            .try_get(5)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "foreign_keys.condeferrable",
                source: DjogiError::from(e),
            })?;
        let initially_deferred: bool =
            row.try_get(6)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "foreign_keys.condeferred",
                    source: DjogiError::from(e),
                })?;
        // A-1: filter HeeRanjID artifact tables in Rust against the
        // sorted allowlist rather than via SQL prefix matching.
        if is_heeranjid_artifact_table(&table) || is_heeranjid_artifact_table(&ref_table) {
            continue;
        }
        let on_delete = match on_delete_code.as_bytes().first().copied() {
            Some(b'r') => OnDeleteSchema::Restrict,
            Some(b'c') => OnDeleteSchema::Cascade,
            Some(b'n') => OnDeleteSchema::SetNull,
            Some(b'd') => OnDeleteSchema::SetDefault,
            Some(b'a') => OnDeleteSchema::NoAction,
            _ => {
                return Err(VerifyRunError::CatalogQueryFailed {
                    query_label: "foreign_keys.confdeltype",
                    source: DjogiError::Db(crate::error::DbError::other(format!(
                        "unexpected pg_constraint.confdeltype value {on_delete_code:?}"
                    ))),
                });
            }
        };
        out.push(LiveForeignKey {
            table,
            column,
            ref_table,
            ref_column,
            on_delete,
            deferrable,
            initially_deferred,
        });
    }
    Ok(out)
}

// ── Diff helpers (snapshot vs. live projection) ───────────────────────────

/// Compare every snapshot table to its live counterpart and emit
/// drift diagnostics.
///
/// Diagnostics emitted:
/// - D601 / D602 — table presence mismatch (Error).
/// - D603 / D604 — column presence mismatch (Error).
/// - D605       — column nullability differs (Error).
/// - D606       — column type-string drift (Warning).
/// - D607       — column DEFAULT differs (Error, B-5).
/// - D608       — primary key column list differs (Error, B-6).
/// - D609       — foreign-key shape differs (Error).
fn diff_tables(
    snapshot: &AppliedSchema,
    live: &AppliedSchema,
    diagnostics: &mut Vec<VerifyDiagnostic>,
) {
    // D601 — snapshot table missing in live DB.
    for name in snapshot.models.keys() {
        if !live.models.contains_key(name) {
            diagnostics.push(VerifyDiagnostic {
                code: "D601".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "table `{name}` exists in snapshot but is missing from \
                     the live database; the schema may have been dropped \
                     out-of-band or the migration that creates it was \
                     never applied",
                ),
                location: Some(name.clone()),
            });
        }
    }

    // D602 — live table not represented in snapshot.
    for name in live.models.keys() {
        if !snapshot.models.contains_key(name) {
            diagnostics.push(VerifyDiagnostic {
                code: "D602".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "table `{name}` exists in the live database but is not \
                     in the snapshot; either an out-of-band migration ran \
                     or the snapshot is stale",
                ),
                location: Some(name.clone()),
            });
        }
    }

    // D603 / D604 / D605 / D606 / D607 — per-column drift for tables
    // present on both sides. D608 — PK shape comparison.
    for (name, snap_table) in &snapshot.models {
        let Some(live_table) = live.models.get(name) else {
            continue;
        };
        let snap_cols: BTreeMap<&str, &ColumnSchema> = snap_table
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c))
            .collect();
        let live_cols: BTreeMap<&str, &ColumnSchema> = live_table
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c))
            .collect();

        // D603 — column missing in live DB.
        for col_name in snap_cols.keys() {
            if !live_cols.contains_key(col_name) {
                diagnostics.push(VerifyDiagnostic {
                    code: "D603".to_string(),
                    severity: VerifySeverity::Error,
                    message: format!(
                        "column `{name}.{col_name}` exists in snapshot but \
                         is missing from the live database",
                    ),
                    location: Some(format!("{name}.{col_name}")),
                });
            }
        }

        // D604 — column in live DB not in snapshot.
        for col_name in live_cols.keys() {
            if !snap_cols.contains_key(col_name) {
                diagnostics.push(VerifyDiagnostic {
                    code: "D604".to_string(),
                    severity: VerifySeverity::Error,
                    message: format!(
                        "column `{name}.{col_name}` exists in the live \
                         database but not in the snapshot",
                    ),
                    location: Some(format!("{name}.{col_name}")),
                });
            }
        }

        // D605 / D606 / D607 — per-column shape comparison on shared
        // columns.
        for (col_name, snap_col) in &snap_cols {
            let Some(live_col) = live_cols.get(col_name) else {
                continue;
            };
            // D605 — nullability drift.
            if snap_col.nullable != live_col.nullable {
                diagnostics.push(VerifyDiagnostic {
                    code: "D605".to_string(),
                    severity: VerifySeverity::Error,
                    message: format!(
                        "column `{name}.{col_name}` nullability differs: \
                         snapshot {snap_n}, live {live_n}",
                        snap_n = snap_col.nullable,
                        live_n = live_col.nullable,
                    ),
                    location: Some(format!("{name}.{col_name}")),
                });
            }

            // D606 — type-string drift, advisory. The catalog
            // rendering ("bigint") is canonicalised by
            // `render_type_for_compare`, so a mismatch after that
            // canonicalisation is a real drift sign — but the
            // snapshot's `sql_type` is operator-authored and may
            // legitimately use a different rendering. Surface as
            // `Warning`.
            let snap_canon = render_type_for_compare(&snap_col.sql_type);
            let live_canon = render_type_for_compare(&live_col.sql_type);
            if snap_canon != live_canon {
                diagnostics.push(VerifyDiagnostic {
                    code: "D606".to_string(),
                    severity: VerifySeverity::Warning,
                    message: format!(
                        "column `{name}.{col_name}` type differs (advisory): \
                         snapshot `{s}`, live `{l}`",
                        s = snap_col.sql_type,
                        l = live_col.sql_type,
                    ),
                    location: Some(format!("{name}.{col_name}")),
                });
            }

            // D607 — column DEFAULT drift (B-5). Both sides compare
            // through `normalize_default_expr` — Postgres canonicalises
            // string defaults as `'foo'::text` even when the operator
            // wrote `'foo'`, so we strip trailing `::TYPE` casts on
            // both sides before equality. Empty `None` on both sides
            // is a clean match. The normalisation strategy is
            // documented on `normalize_default_expr`.
            let snap_default = normalize_default_expr(snap_col.default_sql.as_deref());
            let live_default = normalize_default_expr(live_col.default_sql.as_deref());
            if snap_default != live_default {
                diagnostics.push(VerifyDiagnostic {
                    code: "D607".to_string(),
                    severity: VerifySeverity::Error,
                    message: format!(
                        "column `{name}.{col_name}` DEFAULT differs: \
                         snapshot {s}, live {l}",
                        s = render_default_for_message(&snap_default),
                        l = render_default_for_message(&live_default),
                    ),
                    location: Some(format!("{name}.{col_name}")),
                });
            }

            if snap_col.foreign_key != live_col.foreign_key {
                diagnostics.push(VerifyDiagnostic {
                    code: "D609".to_string(),
                    severity: VerifySeverity::Error,
                    message: format!(
                        "column `{name}.{col_name}` foreign key differs: \
                         snapshot {s:?}, live {l:?}",
                        s = snap_col.foreign_key,
                        l = live_col.foreign_key,
                    ),
                    location: Some(format!("{name}.{col_name}")),
                });
            }
        }

        // D608 — PK column list differs (B-6). Compares the snapshot's
        // declared PK columns against the live PK projection. Cases:
        //   - snapshot has PK, live does not → Error
        //   - snapshot has no PK, live does → Error
        //   - both have PKs but column list (in order) differs → Error
        diff_primary_key(
            name,
            &snap_table.primary_key,
            &live_table.primary_key,
            diagnostics,
        );
    }
}

/// Compare snapshot vs. live primary-key column lists for one table
/// and emit `D608` on any drift. Order-sensitive — composite PKs on
/// `(a, b)` are not equal to PKs on `(b, a)`.
fn diff_primary_key(
    table_name: &str,
    snap_pk: &PrimaryKeySchema,
    live_pk: &PrimaryKeySchema,
    diagnostics: &mut Vec<VerifyDiagnostic>,
) {
    // Both empty column lists is the "no PK on either side" case —
    // a clean match. The live-DB projection populates `columns` with
    // the actual PK column names when a PK exists, and an empty
    // vector when no PK constraint is found.
    let snap_empty = snap_pk.columns.is_empty();
    let live_empty = live_pk.columns.is_empty();

    if snap_empty && live_empty {
        return;
    }
    if snap_empty != live_empty || snap_pk.columns != live_pk.columns {
        diagnostics.push(VerifyDiagnostic {
            code: "D608".to_string(),
            severity: VerifySeverity::Error,
            message: format!(
                "table `{table_name}` primary key differs: \
                 snapshot {snap:?}, live {live:?}",
                snap = snap_pk.columns,
                live = live_pk.columns,
            ),
            location: Some(format!("{table_name}.<pk>")),
        });
    }
}

/// Compare the snapshot's index list to the live projection.
///
/// Diagnostics emitted:
/// - D610 — snapshot index missing in live DB (Error).
/// - D611 — live index not in snapshot (Warning — may be a
///   constraint-backed auto-index).
/// - D612 — index columns differ (Error, B-7).
/// - D613 — index uniqueness differs (Error, B-7).
/// - D614 — index method differs (Warning, B-7).
/// - D615 — index lives on a different table (Error, B-7).
/// - D693 — `INCLUDE` / partial-predicate not yet checked (Info).
fn diff_indexes(
    snapshot: &AppliedSchema,
    live: &AppliedSchema,
    diagnostics: &mut Vec<VerifyDiagnostic>,
) {
    let snap_by_name: BTreeMap<&str, &IndexSchema> = snapshot
        .indexes
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();
    let live_by_name: BTreeMap<&str, &IndexSchema> =
        live.indexes.iter().map(|i| (i.name.as_str(), i)).collect();

    // D610 — snapshot index missing in live DB.
    for name in snap_by_name.keys() {
        if !live_by_name.contains_key(name) {
            diagnostics.push(VerifyDiagnostic {
                code: "D610".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "index `{name}` exists in snapshot but is missing from \
                     the live database",
                ),
                location: Some(format!("index:{name}")),
            });
        }
    }

    // D611 — live index not in snapshot. Auto-created indexes
    // (e.g. ones backing `UNIQUE` constraints) reach the projection;
    // surface as `Warning` because Postgres legitimately creates
    // these for snapshot-declared `UNIQUE` columns / constraints
    // and the snapshot does not record them as separate indexes.
    for name in live_by_name.keys() {
        if !snap_by_name.contains_key(name) {
            diagnostics.push(VerifyDiagnostic {
                code: "D611".to_string(),
                severity: VerifySeverity::Warning,
                message: format!(
                    "index `{name}` exists in the live database but is not \
                     in the snapshot's index list (may be a constraint-backed \
                     auto-index)",
                ),
                location: Some(format!("index:{name}")),
            });
        }
    }

    // D612 / D613 / D614 / D615 — shape comparison on shared names
    // (B-7). For every name present on both sides we compare table,
    // columns (in order), uniqueness, and access method.
    for (name, snap_idx) in &snap_by_name {
        let Some(live_idx) = live_by_name.get(name) else {
            continue;
        };

        // D615 — index is on the wrong table. A name match with a
        // table mismatch is a hard inconsistency: drop+create with
        // matching name is the operator's path.
        if snap_idx.table != live_idx.table {
            diagnostics.push(VerifyDiagnostic {
                code: "D615".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "index `{name}` is on table `{l}` in the live database \
                     but the snapshot declares it on `{s}`",
                    s = snap_idx.table,
                    l = live_idx.table,
                ),
                location: Some(format!("index:{name}")),
            });
        }

        // D612 — column-list drift. We compare the raw column-name
        // sequence; opclass / order / nulls are not yet projected
        // from live (T8 territory) so we narrow the comparison to
        // the column names in order.
        let snap_cols = index_target_column_names(&snap_idx.target);
        let live_cols = index_target_column_names(&live_idx.target);
        if snap_cols != live_cols {
            diagnostics.push(VerifyDiagnostic {
                code: "D612".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "index `{name}` columns differ: snapshot {s:?}, live {l:?}",
                    s = snap_cols,
                    l = live_cols,
                ),
                location: Some(format!("index:{name}")),
            });
        }

        // D613 — uniqueness drift. The snapshot's `UniqueConstraint`
        // and `UniqueIndex` both project to "unique" when read from
        // the live catalog (`pg_index.indisunique = true`); we treat
        // them as the same uniqueness class for the comparison.
        let snap_unique = matches!(
            snap_idx.kind,
            IndexKindSchema::UniqueIndex | IndexKindSchema::UniqueConstraint
        );
        let live_unique = matches!(
            live_idx.kind,
            IndexKindSchema::UniqueIndex | IndexKindSchema::UniqueConstraint
        );
        if snap_unique != live_unique {
            diagnostics.push(VerifyDiagnostic {
                code: "D613".to_string(),
                severity: VerifySeverity::Error,
                message: format!(
                    "index `{name}` uniqueness differs: snapshot {s}, live {l}",
                    s = snap_unique,
                    l = live_unique,
                ),
                location: Some(format!("index:{name}")),
            });
        }

        // D614 — access method drift, advisory. A method change
        // (e.g. btree vs gin) is meaningful but the snapshot's
        // declaration is the operator's source of truth — surface
        // as Warning so the operator can investigate without
        // failing CI.
        if snap_idx.index_type != live_idx.index_type {
            diagnostics.push(VerifyDiagnostic {
                code: "D614".to_string(),
                severity: VerifySeverity::Warning,
                message: format!(
                    "index `{name}` method differs: snapshot {s:?}, live {l:?}",
                    s = snap_idx.index_type,
                    l = live_idx.index_type,
                ),
                location: Some(format!("index:{name}")),
            });
        }

        // D693 — INCLUDE columns / partial-predicate are not yet
        // projected from live. Surface as Info so the operator sees
        // exactly what is and is not covered. T8 tightens this.
        if !snap_idx.include.is_empty() || snap_idx.predicate.is_some() {
            diagnostics.push(VerifyDiagnostic {
                code: "D693".to_string(),
                severity: VerifySeverity::Info,
                message: format!(
                    "index `{name}` declares INCLUDE / partial-predicate; T5 \
                     verify does not yet project these from the live catalog \
                     (deferred to T8)",
                ),
                location: Some(format!("index:{name}")),
            });
        }
    }
}

/// Extract the column-name sequence from an [`IndexTargetSchema`].
/// Expression-form indexes return an empty Vec — those compare
/// equal only when both sides are expression-form, which is fine for
/// T5's coarse-grained comparison.
fn index_target_column_names(target: &IndexTargetSchema) -> Vec<&str> {
    match target {
        IndexTargetSchema::Columns(cols) => cols.iter().map(|c| c.name.as_str()).collect(),
        IndexTargetSchema::Expression(_) => Vec::new(),
    }
}

/// Canonicalise a column DEFAULT expression for comparison (B-5).
///
/// **Strategy.** Postgres typically renders defaults with explicit
/// type casts (`'foo'::text`, `now()::timestamp with time zone`).
/// Operators authoring snapshots often write the bare form
/// (`'foo'`, `now()`). We strip ALL trailing `::<type>` casts on
/// both sides so equivalent expressions compare equal:
///
/// - `'foo'` vs `'foo'::text` → match
/// - `now()` vs `now()::timestamptz` → match
/// - `'foo'::text::varchar` → `'foo'` (nested casts collapsed in a
///   loop — Codex round-2 B-5 fix; Postgres renders nested casts
///   unchanged on `pg_get_expr`, so the comparator must peel them)
/// - `42` vs `43` → mismatch (different value)
/// - `now()` vs `current_timestamp` → mismatch (different func — T8
///   may add an alias map)
///
/// Trim is whitespace-only on both ends; other whitespace inside
/// the expression is preserved (Postgres preserves it on the way
/// back to the catalog).
///
/// `None` and the empty string are normalised to `None` so a column
/// declared with `DEFAULT NULL` (which Postgres collapses to "no
/// default") matches a snapshot that omits the field entirely.
fn normalize_default_expr(expr: Option<&str>) -> Option<String> {
    let raw = expr?.trim();
    if raw.is_empty() {
        return None;
    }
    // Strip ALL trailing `::<type>` casts. We loop, peeling one
    // trailing cast per iteration, until the expression no longer
    // ends with a cast. Each iteration scans the current expression
    // forward, tracking the LAST `::` that is not inside a quoted
    // string. The byte-level forward scan with a single-quote
    // toggle skips over `::` sequences inside quoted strings; no
    // pattern-matching engine is involved.
    //
    // Loop termination: each iteration either strips at least one
    // byte (`raw[..idx]` where `idx < raw.len()`) or breaks. The
    // length monotonically decreases, so the loop terminates.
    let mut current = raw.to_string();
    loop {
        let bytes = current.as_bytes();
        let mut in_string = false;
        let mut last_double_colon: Option<usize> = None;
        let mut i = 0usize;
        while i + 1 < bytes.len() {
            let b = bytes[i];
            if b == b'\'' {
                // SQL strings escape a literal single-quote by doubling
                // it. The toggle below treats `''` as a same-state
                // sequence: enter the inner state on the first quote,
                // immediately leave on the second — net result: still
                // outside the string.
                in_string = !in_string;
            } else if !in_string && b == b':' && bytes[i + 1] == b':' {
                last_double_colon = Some(i);
                i += 2;
                continue;
            }
            i += 1;
        }
        match last_double_colon {
            Some(idx) => {
                let trimmed = current[..idx].trim_end();
                if trimmed.is_empty() {
                    return None;
                }
                let next = trimmed.to_string();
                if next == current {
                    // Defensive: zero-length strip should be impossible
                    // here (`idx < bytes.len()` and `trim_end` only
                    // shrinks), but break rather than spin.
                    break;
                }
                current = next;
            }
            None => break,
        }
    }
    if current.is_empty() {
        None
    } else {
        Some(current)
    }
}

/// Render a normalised default expression for inclusion in a
/// diagnostic message. `None` shows as `<no default>` so the
/// operator-facing message is unambiguous.
fn render_default_for_message(d: &Option<String>) -> String {
    match d {
        Some(s) => format!("`{s}`"),
        None => "<no default>".to_string(),
    }
}

/// Surface advisory `Info` diagnostics for snapshot fields the T5
/// projection does not yet check. Operators see exactly what is
/// covered and what is deferred.
fn diff_advisory_fields(snapshot: &AppliedSchema, diagnostics: &mut Vec<VerifyDiagnostic>) {
    // D690 — FTS configuration is not checked against live triggers.
    let fts_tables: Vec<&str> = snapshot
        .models
        .iter()
        .filter_map(|(n, t)| t.fts.as_ref().map(|_| n.as_str()))
        .collect();
    if !fts_tables.is_empty() {
        let location = fts_tables.first().copied().map(|s| s.to_string());
        diagnostics.push(VerifyDiagnostic {
            code: "D690".to_string(),
            severity: VerifySeverity::Info,
            message: format!(
                "{n} table(s) declare FTS configuration; T5 verify does not \
                 yet check FTS triggers / generated columns against the live \
                 catalog (deferred to T8)",
                n = fts_tables.len(),
            ),
            location,
        });
    }

    // D691 — partition shape is not checked.
    let partitioned: Vec<&str> = snapshot
        .models
        .iter()
        .filter_map(|(n, t)| t.partition.as_ref().map(|_| n.as_str()))
        .collect();
    if !partitioned.is_empty() {
        let location = partitioned.first().copied().map(|s| s.to_string());
        diagnostics.push(VerifyDiagnostic {
            code: "D691".to_string(),
            severity: VerifySeverity::Info,
            message: format!(
                "{n} table(s) declare a partition strategy; T5 verify does \
                 not yet check partition method / column against the live \
                 catalog (deferred to T8)",
                n = partitioned.len(),
            ),
            location,
        });
    }

    // D692 — enums are not checked.
    if !snapshot.enums.is_empty() {
        diagnostics.push(VerifyDiagnostic {
            code: "D692".to_string(),
            severity: VerifySeverity::Info,
            message: format!(
                "{n} enum type(s) declared; T5 verify does not yet check \
                 enum variants against the live `pg_enum` catalog (deferred \
                 to T8)",
                n = snapshot.enums.len(),
            ),
            location: None,
        });
    }
}

/// Canonicalise a Postgres type rendering for comparison. The
/// snapshot stores `BIGINT`, `TEXT`, `VARCHAR(255)` (uppercase,
/// no aliases); `format_type` from the catalog returns lowercase
/// (`bigint`, `text`, `character varying(255)`). We normalise to
/// lowercase + map the well-known aliases.
///
/// **Implementation: byte-level lowercasing followed by explicit
/// substring substitution against a fixed alias table.** No
/// pattern-matching engine is involved — every comparison is
/// either an exact byte-equality check or a fixed-string `replace`
/// call.
fn render_type_for_compare(s: &str) -> String {
    let lower: String = s
        .as_bytes()
        .iter()
        .map(|b| b.to_ascii_lowercase() as char)
        .collect();
    // Common alias collapses. Order matters: longer aliases first so
    // a substring of one alias does not match another.
    let aliases: &[(&str, &str)] = &[
        ("character varying", "varchar"),
        ("timestamp with time zone", "timestamptz"),
        ("timestamp without time zone", "timestamp"),
        ("integer", "int4"),
        ("bigint", "int8"),
        ("smallint", "int2"),
        ("double precision", "float8"),
        ("real", "float4"),
        ("boolean", "bool"),
    ];
    let mut out = lower;
    for (from, to) in aliases {
        out = out.replace(from, to);
    }
    out
}

// ── Internal accessors used by repair / baseline / T8 ────────────────────

/// Live-DB projection accessor — entry point for code paths that
/// need the projection without the verify-side diff. Used by
/// [`super::runner::baseline_plan`] (B-11) and
/// [`super::repair::repair_snapshot_rebuild`] (B-12); reserved for
/// T8's tightened verify diagnostics.
///
/// **Bucket scoping (Codex round-2 B-11).** The projection is scoped
/// to the supplied [`BucketKey`] so an app's baseline / rebuild does
/// not pull in another app's tables. Postgres has no per-app schema
/// concept (every app's tables live in `public`), so the scoping is
/// driven from the inventory's `ModelDescriptor::app` field:
///
/// - **Synthetic global bucket** (`bucket.app == ""`,
///   [`crate::AppDescriptor::GLOBAL_LABEL`]): every live table is
///   included EXCEPT those whose `ModelDescriptor` declares a
///   non-global app. The empty-label bucket is the catch-all for
///   tables that pre-date Djogi's apps subsystem (legacy / baseline
///   adoption) plus any model that omitted `#[model(app = ...)]`.
/// - **Named bucket** (`bucket.app == "billing"`, etc.): only tables
///   whose `ModelDescriptor` declares this exact app label are
///   projected. Live tables that have no inventory descriptor are
///   excluded — they belong to either the global bucket or another
///   app's baseline, never to a named app's projection.
///
/// **Where the bucket database flows.** The `database` component is
/// already routed by the caller via `DjogiContext::switch_to(...)`
/// before calling this function — `ctx` is always pointing at the
/// right pool. The projection itself only ever queries the
/// connection it is given; the bucket database is advisory at this
/// layer (it is checked in the caller against the routed pool).
///
/// **Why inventory and not the ledger.** The ledger only records
/// migration versions, not the tables those migrations touched. A
/// bucket-scoped projection driven from `app_label` history would
/// require a per-migration table-touch index that does not exist
/// today. Inventory-driven scoping is the project-wide convention
/// the migration substrate already uses (see
/// [`super::projection::project_from_inventory`]); reusing it keeps
/// the two projection paths in lockstep.
pub(super) async fn live_schema_for_repair(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
) -> Result<AppliedSchema, VerifyRunError> {
    let mut full = project_live_schema(ctx).await?;

    // Build the set of table names declared in inventory, grouped by
    // app label. We only walk inventory once and produce two sets:
    //   - this_bucket_tables: tables whose descriptor declares the
    //     supplied bucket's app label.
    //   - all_app_tables: tables whose descriptor declares ANY
    //     non-global app label.
    // Both sets compare on Postgres table name (`ModelDescriptor::
    // table_name`) so the live projection's BTreeMap key lines up.
    use crate::AppDescriptor;
    use crate::descriptor::ModelDescriptor;
    let mut this_bucket_tables: std::collections::BTreeSet<&str> =
        std::collections::BTreeSet::new();
    let mut all_app_tables: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for m in inventory::iter::<ModelDescriptor> {
        let label = m.app.unwrap_or(AppDescriptor::GLOBAL_LABEL);
        if label == bucket.app.as_str() {
            this_bucket_tables.insert(m.table_name);
        }
        if label != AppDescriptor::GLOBAL_LABEL {
            all_app_tables.insert(m.table_name);
        }
    }

    let is_global_bucket = bucket.app.as_str() == AppDescriptor::GLOBAL_LABEL;
    full.models.retain(|table_name, _| {
        if is_global_bucket {
            // Global bucket: include the table unless an inventory
            // descriptor explicitly assigns it to a different app.
            !all_app_tables.contains(table_name.as_str())
        } else {
            // Named bucket: include only tables whose descriptor
            // matches this app label.
            this_bucket_tables.contains(table_name.as_str())
        }
    });

    // Indexes are flat-listed; filter to those whose `table` is still
    // in the (post-filter) models set so the projection stays
    // self-consistent.
    let kept_tables: std::collections::BTreeSet<String> = full.models.keys().cloned().collect();
    full.indexes.retain(|i| kept_tables.contains(&i.table));

    // Record the bucket's app label as the sole `registered_apps`
    // entry so a downstream consumer that re-runs the differ against
    // this projection sees the same bucket boundary.
    full.registered_apps = vec![bucket.app.clone()];

    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::schema::{
        ColumnSchema, ForeignKeySchema, IndexKindSchema, IndexSchema, IndexTargetSchema,
        IndexTypeSchema, OnDeleteSchema, PkKindSchema, PrimaryKeySchema, RelationKindSchema,
        TableSchema,
    };

    fn empty_snapshot() -> AppliedSchema {
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: super::super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        }
    }

    fn col(name: &str, sql_type: &str, nullable: bool) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: name.to_string(),
            nullable,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: sql_type.to_string(),
            unique: false,
        }
    }

    fn fk_column(
        name: &str,
        ref_table: &str,
        ref_column: &str,
        on_delete: OnDeleteSchema,
        deferrable: bool,
        initially_deferred: bool,
    ) -> ColumnSchema {
        ColumnSchema {
            foreign_key: Some(ForeignKeySchema {
                deferrable,
                initially_deferred,
                on_delete,
                ref_column: ref_column.to_string(),
                ref_table: ref_table.to_string(),
            }),
            on_delete: Some(on_delete),
            relation_kind: Some(RelationKindSchema::ForeignKey),
            ..col(name, "BIGINT", false)
        }
    }

    fn table(name: &str, columns: Vec<ColumnSchema>) -> TableSchema {
        TableSchema {
            app: None,
            columns,
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
            table: name.to_string(),
            tenant_key: None,
        }
    }

    fn idx(name: &str, table: &str, unique: bool) -> IndexSchema {
        IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: if unique {
                IndexKindSchema::UniqueIndex
            } else {
                IndexKindSchema::NonUnique
            },
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: table.to_string(),
            target: IndexTargetSchema::Columns(Vec::new()),
        }
    }

    // ── Sort key determinism ─────────────────────────────────────────────

    #[test]
    fn diagnostics_sort_by_code_then_location() {
        // The contents are non-trivial but the array is fixed-size at
        // construction; clippy's `useless_vec` lint nudges us to use
        // an array, and the subsequent `sort_by_key` works on slices
        // so the array shape is fine.
        let mut diagnostics = [
            VerifyDiagnostic {
                code: "D602".to_string(),
                severity: VerifySeverity::Error,
                message: "msg".to_string(),
                location: Some("zebra".to_string()),
            },
            VerifyDiagnostic {
                code: "D601".to_string(),
                severity: VerifySeverity::Error,
                message: "msg".to_string(),
                location: Some("alpha".to_string()),
            },
            VerifyDiagnostic {
                code: "D601".to_string(),
                severity: VerifySeverity::Error,
                message: "msg".to_string(),
                location: Some("beta".to_string()),
            },
        ];
        diagnostics.sort_by_key(|d| d.sort_key());
        let codes: Vec<(&str, &str)> = diagnostics
            .iter()
            .map(|d| (d.code.as_str(), d.location.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            codes,
            vec![("D601", "alpha"), ("D601", "beta"), ("D602", "zebra")]
        );
    }

    // ── diff_tables ──────────────────────────────────────────────────────

    #[test]
    fn diff_tables_clean_match_emits_no_diagnostics() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "users".to_string(),
            table("users", vec![col("id", "BIGINT", false)]),
        );
        let mut live = empty_snapshot();
        live.models.insert(
            "users".to_string(),
            table("users", vec![col("id", "bigint", false)]),
        );
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        assert!(
            diagnostics.is_empty(),
            "clean match must emit no diagnostics; got {diagnostics:?}"
        );
    }

    #[test]
    fn diff_tables_surfaces_missing_table_as_d601() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "users".to_string(),
            table("users", vec![col("id", "BIGINT", false)]),
        );
        let live = empty_snapshot();
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "D601");
        assert_eq!(diagnostics[0].severity, VerifySeverity::Error);
        assert_eq!(diagnostics[0].location.as_deref(), Some("users"));
    }

    #[test]
    fn diff_tables_surfaces_extra_live_table_as_d602() {
        let snap = empty_snapshot();
        let mut live = empty_snapshot();
        live.models.insert(
            "widgets".to_string(),
            table("widgets", vec![col("id", "bigint", false)]),
        );
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "D602");
        assert_eq!(diagnostics[0].severity, VerifySeverity::Error);
    }

    #[test]
    fn diff_tables_surfaces_missing_column_as_d603() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "users".to_string(),
            table(
                "users",
                vec![col("id", "BIGINT", false), col("email", "TEXT", false)],
            ),
        );
        let mut live = empty_snapshot();
        live.models.insert(
            "users".to_string(),
            table("users", vec![col("id", "bigint", false)]),
        );
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        let by_code: BTreeMap<_, _> = diagnostics.iter().map(|d| (d.code.as_str(), d)).collect();
        assert!(by_code.contains_key("D603"));
        assert_eq!(by_code["D603"].location.as_deref(), Some("users.email"));
    }

    #[test]
    fn diff_tables_surfaces_extra_live_column_as_d604() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "users".to_string(),
            table("users", vec![col("id", "BIGINT", false)]),
        );
        let mut live = empty_snapshot();
        live.models.insert(
            "users".to_string(),
            table(
                "users",
                vec![col("id", "bigint", false), col("rogue", "text", true)],
            ),
        );
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        let by_code: BTreeMap<_, _> = diagnostics.iter().map(|d| (d.code.as_str(), d)).collect();
        assert!(by_code.contains_key("D604"));
        assert_eq!(by_code["D604"].location.as_deref(), Some("users.rogue"));
    }

    #[test]
    fn diff_tables_surfaces_nullability_drift_as_d605() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "users".to_string(),
            table("users", vec![col("email", "TEXT", false)]),
        );
        let mut live = empty_snapshot();
        live.models.insert(
            "users".to_string(),
            table("users", vec![col("email", "text", true)]),
        );
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        assert!(diagnostics.iter().any(|d| d.code == "D605"));
    }

    #[test]
    fn diff_tables_surfaces_type_drift_as_advisory_d606() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "users".to_string(),
            table("users", vec![col("age", "INTEGER", true)]),
        );
        let mut live = empty_snapshot();
        live.models.insert(
            "users".to_string(),
            table("users", vec![col("age", "BIGINT", true)]),
        );
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        let d606 = diagnostics
            .iter()
            .find(|d| d.code == "D606")
            .expect("D606 expected");
        assert_eq!(d606.severity, VerifySeverity::Warning);
    }

    #[test]
    fn diff_tables_surfaces_foreign_key_drift_as_d609() {
        let mut snap = empty_snapshot();
        snap.models.insert(
            "posts".to_string(),
            table(
                "posts",
                vec![
                    col("id", "BIGINT", false),
                    fk_column(
                        "author_id",
                        "users",
                        "id",
                        OnDeleteSchema::Restrict,
                        true,
                        true,
                    ),
                ],
            ),
        );
        let mut live = empty_snapshot();
        live.models.insert(
            "posts".to_string(),
            table(
                "posts",
                vec![
                    col("id", "bigint", false),
                    fk_column(
                        "author_id",
                        "users",
                        "id",
                        OnDeleteSchema::Restrict,
                        true,
                        false,
                    ),
                ],
            ),
        );

        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);

        let d609 = diagnostics
            .iter()
            .find(|d| d.code == "D609")
            .expect("D609 expected");
        assert_eq!(d609.severity, VerifySeverity::Error);
        assert_eq!(d609.location.as_deref(), Some("posts.author_id"));
    }

    // ── diff_indexes ─────────────────────────────────────────────────────

    #[test]
    fn diff_indexes_missing_in_live_is_error() {
        let mut snap = empty_snapshot();
        snap.indexes.push(idx("users_email_idx", "users", false));
        let live = empty_snapshot();
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        assert!(diagnostics.iter().any(|d| d.code == "D610"));
    }

    #[test]
    fn diff_indexes_extra_in_live_is_warning() {
        let snap = empty_snapshot();
        let mut live = empty_snapshot();
        live.indexes.push(idx("users_email_idx", "users", true));
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        let d611 = diagnostics
            .iter()
            .find(|d| d.code == "D611")
            .expect("D611 expected");
        assert_eq!(d611.severity, VerifySeverity::Warning);
    }

    // ── render_type_for_compare ──────────────────────────────────────────

    #[test]
    fn render_type_aliases_match_pg_renderings() {
        assert_eq!(render_type_for_compare("BIGINT"), "int8");
        assert_eq!(render_type_for_compare("bigint"), "int8");
        assert_eq!(
            render_type_for_compare("character varying(255)"),
            "varchar(255)"
        );
        assert_eq!(
            render_type_for_compare("timestamp with time zone"),
            "timestamptz"
        );
        assert_eq!(render_type_for_compare("BOOLEAN"), "bool");
        assert_eq!(render_type_for_compare("INTEGER"), "int4");
        assert_eq!(render_type_for_compare("SMALLINT"), "int2");
        assert_eq!(render_type_for_compare("DOUBLE PRECISION"), "float8");
        assert_eq!(render_type_for_compare("REAL"), "float4");
    }

    #[test]
    fn render_type_preserves_unknown_names_lowercase() {
        assert_eq!(render_type_for_compare("CITEXT"), "citext");
    }

    // ── VerifyReport accessors ──────────────────────────────────────────

    #[test]
    fn verify_report_has_errors_detects_error_diagnostics() {
        let r = VerifyReport {
            diagnostics: vec![VerifyDiagnostic {
                code: "D601".to_string(),
                severity: VerifySeverity::Error,
                message: "x".to_string(),
                location: None,
            }],
            latest_applied_version: None,
            applied_count: 0,
            unfinished_count: 0,
        };
        assert!(r.has_errors());
        assert!(!r.has_warnings());
    }

    #[test]
    fn verify_report_has_warnings_detects_warning_diagnostics() {
        let r = VerifyReport {
            diagnostics: vec![VerifyDiagnostic {
                code: "D606".to_string(),
                severity: VerifySeverity::Warning,
                message: "x".to_string(),
                location: None,
            }],
            latest_applied_version: None,
            applied_count: 0,
            unfinished_count: 0,
        };
        assert!(!r.has_errors());
        assert!(r.has_warnings());
    }

    #[test]
    fn verify_report_clean_run_reports_neither() {
        let r = VerifyReport {
            diagnostics: Vec::new(),
            latest_applied_version: None,
            applied_count: 0,
            unfinished_count: 0,
        };
        assert!(!r.has_errors());
        assert!(!r.has_warnings());
    }

    // ── Advisory diagnostics ─────────────────────────────────────────────

    #[test]
    fn advisory_emits_d692_when_enums_present() {
        let mut snap = empty_snapshot();
        snap.enums.insert(
            "status".to_string(),
            super::super::schema::EnumSchema {
                name: "status".to_string(),
                variants: vec!["active".to_string()],
            },
        );
        let mut diagnostics = Vec::new();
        diff_advisory_fields(&snap, &mut diagnostics);
        assert!(diagnostics.iter().any(|d| d.code == "D692"));
    }

    // ── normalize_default_expr (B-5) ─────────────────────────────────────

    #[test]
    fn normalize_default_strips_trailing_text_cast() {
        // 'foo'::text round-trips to 'foo' so a snapshot that wrote
        // 'foo' compares equal to a live catalog that rendered it
        // with the cast.
        assert_eq!(
            normalize_default_expr(Some("'foo'::text")),
            Some("'foo'".to_string())
        );
    }

    #[test]
    fn normalize_default_strips_trailing_timestamptz_cast() {
        assert_eq!(
            normalize_default_expr(Some("now()::timestamp with time zone")),
            Some("now()".to_string())
        );
    }

    #[test]
    fn normalize_default_preserves_unsuffixed_expr() {
        assert_eq!(
            normalize_default_expr(Some("now()")),
            Some("now()".to_string())
        );
        assert_eq!(normalize_default_expr(Some("42")), Some("42".to_string()));
    }

    #[test]
    fn normalize_default_treats_none_and_empty_as_no_default() {
        assert_eq!(normalize_default_expr(None), None);
        assert_eq!(normalize_default_expr(Some("")), None);
        assert_eq!(normalize_default_expr(Some("   ")), None);
    }

    #[test]
    fn normalize_default_preserves_double_colons_inside_string() {
        // The double-colon scanner toggles `in_string` on each single
        // quote so a literal `::` inside a quoted string is preserved.
        // This test pins the canonical case: a default like
        // `'a::b'::text` should normalise to `'a::b'`.
        assert_eq!(
            normalize_default_expr(Some("'a::b'::text")),
            Some("'a::b'".to_string())
        );
    }

    #[test]
    fn normalize_default_strips_nested_casts() {
        // Codex round-2 B-5 follow-up: the previous implementation
        // peeled exactly ONE trailing `::TYPE`. For nested casts —
        // legitimate when an adopter writes `'foo'::text::varchar`
        // and Postgres renders it back unchanged — only the outermost
        // cast was stripped, leaving `'foo'::text` and producing a
        // spurious D607. The fix loops the strip step until the
        // expression no longer ends with a cast.
        assert_eq!(
            normalize_default_expr(Some("'foo'::text::varchar")),
            Some("'foo'".to_string())
        );
        assert_eq!(
            normalize_default_expr(Some("123::int::bigint::numeric")),
            Some("123".to_string())
        );
        // Whitespace between the casts is also tolerated — Postgres
        // does not emit it but the comparator should not be brittle.
        assert_eq!(
            normalize_default_expr(Some("'x'::text  ::  varchar")),
            Some("'x'".to_string())
        );
    }

    // ── diff_primary_key (B-6) ──────────────────────────────────────────

    #[test]
    fn diff_pk_match_emits_no_diagnostic() {
        let mut diagnostics = Vec::new();
        let snap = PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        };
        let live = PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        };
        diff_primary_key("users", &snap, &live, &mut diagnostics);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diff_pk_snapshot_has_pk_live_does_not_emits_d608() {
        let mut diagnostics = Vec::new();
        let snap = PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        };
        let live = PrimaryKeySchema {
            columns: Vec::new(),
            kind: PkKindSchema::None,
        };
        diff_primary_key("users", &snap, &live, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "D608");
        assert_eq!(diagnostics[0].severity, VerifySeverity::Error);
    }

    #[test]
    fn diff_pk_live_has_pk_snapshot_does_not_emits_d608() {
        let mut diagnostics = Vec::new();
        let snap = PrimaryKeySchema {
            columns: Vec::new(),
            kind: PkKindSchema::None,
        };
        let live = PrimaryKeySchema {
            columns: vec!["id".to_string()],
            kind: PkKindSchema::HeerId,
        };
        diff_primary_key("users", &snap, &live, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "D608");
    }

    #[test]
    fn diff_pk_column_list_mismatch_emits_d608() {
        let mut diagnostics = Vec::new();
        let snap = PrimaryKeySchema {
            columns: vec!["a".to_string(), "b".to_string()],
            kind: PkKindSchema::Composite,
        };
        let live = PrimaryKeySchema {
            columns: vec!["b".to_string(), "a".to_string()],
            kind: PkKindSchema::Composite,
        };
        diff_primary_key("t", &snap, &live, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "D608");
    }

    // ── diff_indexes shape mismatch (B-7) ────────────────────────────────

    fn idx_with_columns(name: &str, table: &str, cols: &[&str]) -> IndexSchema {
        IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::NonUnique,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table: table.to_string(),
            target: IndexTargetSchema::Columns(
                cols.iter()
                    .map(|c| super::super::schema::IndexColumnSchema {
                        name: c.to_string(),
                        nulls: super::super::schema::IndexNullsOrderSchema::Default,
                        opclass: None,
                        order: super::super::schema::IndexOrderSchema::Asc,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn diff_indexes_wrong_table_is_d615_error() {
        let mut snap = empty_snapshot();
        let mut live = empty_snapshot();
        snap.indexes
            .push(idx_with_columns("idx_x", "users", &["email"]));
        live.indexes
            .push(idx_with_columns("idx_x", "orders", &["email"]));
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "D615" && d.severity == VerifySeverity::Error)
        );
    }

    #[test]
    fn diff_indexes_wrong_columns_is_d612_error() {
        let mut snap = empty_snapshot();
        let mut live = empty_snapshot();
        snap.indexes
            .push(idx_with_columns("idx_x", "users", &["email"]));
        live.indexes
            .push(idx_with_columns("idx_x", "users", &["name"]));
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "D612" && d.severity == VerifySeverity::Error)
        );
    }

    #[test]
    fn diff_indexes_wrong_uniqueness_is_d613_error() {
        let mut snap = empty_snapshot();
        let mut live = empty_snapshot();
        let mut s = idx_with_columns("idx_x", "users", &["email"]);
        s.kind = IndexKindSchema::UniqueIndex;
        snap.indexes.push(s);
        live.indexes
            .push(idx_with_columns("idx_x", "users", &["email"]));
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "D613" && d.severity == VerifySeverity::Error)
        );
    }

    #[test]
    fn diff_indexes_method_drift_is_d614_warning() {
        let mut snap = empty_snapshot();
        let mut live = empty_snapshot();
        let mut s = idx_with_columns("idx_x", "users", &["email"]);
        s.index_type = IndexTypeSchema::Gin;
        snap.indexes.push(s);
        live.indexes
            .push(idx_with_columns("idx_x", "users", &["email"]));
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "D614" && d.severity == VerifySeverity::Warning)
        );
    }

    #[test]
    fn diff_indexes_clean_match_emits_no_shape_diagnostic() {
        let mut snap = empty_snapshot();
        let mut live = empty_snapshot();
        snap.indexes
            .push(idx_with_columns("idx_x", "users", &["email"]));
        live.indexes
            .push(idx_with_columns("idx_x", "users", &["email"]));
        let mut diagnostics = Vec::new();
        diff_indexes(&snap, &live, &mut diagnostics);
        // No D612 / D613 / D614 / D615 diagnostics on a clean match.
        for code in ["D612", "D613", "D614", "D615"] {
            assert!(
                !diagnostics.iter().any(|d| d.code == code),
                "unexpected {code} on clean match: {diagnostics:?}"
            );
        }
    }

    // ── diff_tables D607 (B-5) ──────────────────────────────────────────

    #[test]
    fn diff_tables_default_drift_emits_d607() {
        let mut snap = empty_snapshot();
        let mut snap_col = col("created_at", "TIMESTAMPTZ", false);
        snap_col.default_sql = Some("now()".to_string());
        snap.models
            .insert("users".to_string(), table("users", vec![snap_col]));
        let mut live = empty_snapshot();
        let live_col = col("created_at", "timestamptz", false);
        // No default.
        live.models
            .insert("users".to_string(), table("users", vec![live_col]));
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code == "D607" && d.severity == VerifySeverity::Error),
            "expected D607 on default drift; got {diagnostics:?}"
        );
    }

    #[test]
    fn diff_tables_default_canonicalisation_treats_text_cast_as_match() {
        // Snapshot wrote "'active'", live catalog rendered it as
        // "'active'::text" — equivalent after normalisation, no D607.
        let mut snap = empty_snapshot();
        let mut snap_col = col("status", "TEXT", false);
        snap_col.default_sql = Some("'active'".to_string());
        snap.models
            .insert("users".to_string(), table("users", vec![snap_col]));
        let mut live = empty_snapshot();
        let mut live_col = col("status", "text", false);
        live_col.default_sql = Some("'active'::text".to_string());
        live.models
            .insert("users".to_string(), table("users", vec![live_col]));
        let mut diagnostics = Vec::new();
        diff_tables(&snap, &live, &mut diagnostics);
        assert!(
            !diagnostics.iter().any(|d| d.code == "D607"),
            "no D607 expected on canonicalised default; got {diagnostics:?}"
        );
    }

    // ── HeeRanjID artifact allowlist (A-1) ──────────────────────────────

    #[test]
    fn heeranjid_allowlist_is_sorted() {
        // binary_search requires sorted input; pin that invariant.
        let mut sorted = HEERANJID_ARTIFACT_TABLES.to_vec();
        sorted.sort();
        assert_eq!(sorted.as_slice(), HEERANJID_ARTIFACT_TABLES);
    }

    #[test]
    fn heeranjid_allowlist_recognises_known_substrate_tables() {
        for name in HEERANJID_ARTIFACT_TABLES {
            assert!(
                is_heeranjid_artifact_table(name),
                "{name} should be in allowlist"
            );
        }
    }

    #[test]
    fn heeranjid_allowlist_does_not_match_adopter_heer_prefix_tables() {
        // The previous LIKE-based exclusion swallowed adopter-owned
        // tables that legitimately started with `heer_`. Confirm the
        // allowlist does not. (Codex round-2 A-1: the spec example
        // names this table `heer_orders` — an adopter's "orders"
        // table that legitimately carries the `heer_` prefix.)
        assert!(!is_heeranjid_artifact_table("heer_user"));
        assert!(!is_heeranjid_artifact_table("heer_orders"));
        assert!(!is_heeranjid_artifact_table("heer"));
        assert!(!is_heeranjid_artifact_table(""));
    }

    // ── T7: emit_out_of_order_diagnostics (D622) ─────────────────────────

    fn ledger_row_at(version: &str, app: &str, ooo: bool, status: LedgerStatus) -> LedgerRow {
        LedgerRow {
            version: version.to_string(),
            description: String::new(),
            checksum_up: "V1:0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
            checksum_down: None,
            execution_mode: super::super::ledger::ExecutionMode::Transactional,
            status,
            execution_time_ms: 0,
            out_of_order_flag: ooo,
            applied_steps_count: 0,
            total_steps: None,
            partial_apply_note: None,
            run_id: 0,
            snapshot_version: super::super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
            app_label: app.to_string(),
        }
    }

    #[test]
    fn d622_warning_for_ooo_row_default_policy() {
        let rows = vec![
            ledger_row_at(
                "V20260201000000__early",
                "billing",
                true,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260301000000__later",
                "billing",
                false,
                LedgerStatus::Applied,
            ),
        ];
        let policy = crate::config::PolicyConfig::default();
        let mut diagnostics = Vec::new();
        emit_out_of_order_diagnostics(&rows, &policy, &mut diagnostics);
        let d622: Vec<&VerifyDiagnostic> =
            diagnostics.iter().filter(|d| d.code == "D622").collect();
        assert_eq!(d622.len(), 1, "exactly one D622 expected");
        assert_eq!(d622[0].severity, VerifySeverity::Warning);
        assert!(
            d622[0].message.contains("V20260201000000__early"),
            "must name the offending row: {}",
            d622[0].message
        );
        assert!(
            d622[0].message.contains("V20260301000000__later"),
            "must name the conflicting peer: {}",
            d622[0].message
        );
    }

    #[test]
    fn d622_error_for_ooo_row_strict_policy() {
        let rows = vec![
            ledger_row_at(
                "V20260201000000__early",
                "billing",
                true,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260301000000__later",
                "billing",
                false,
                LedgerStatus::Applied,
            ),
        ];
        let policy = crate::config::PolicyConfig {
            strict_out_of_order: true,
        };
        let mut diagnostics = Vec::new();
        emit_out_of_order_diagnostics(&rows, &policy, &mut diagnostics);
        let d622: Vec<&VerifyDiagnostic> =
            diagnostics.iter().filter(|d| d.code == "D622").collect();
        assert_eq!(d622.len(), 1);
        assert_eq!(d622[0].severity, VerifySeverity::Error);
    }

    #[test]
    fn d622_silent_when_no_rows_have_ooo_flag() {
        let rows = vec![
            ledger_row_at(
                "V20260101000000__a",
                "billing",
                false,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260201000000__b",
                "billing",
                false,
                LedgerStatus::Applied,
            ),
        ];
        let policy = crate::config::PolicyConfig::default();
        let mut diagnostics = Vec::new();
        emit_out_of_order_diagnostics(&rows, &policy, &mut diagnostics);
        assert!(
            !diagnostics.iter().any(|d| d.code == "D622"),
            "no D622 expected when no rows are out-of-order"
        );
    }

    #[test]
    fn d622_one_per_ooo_row() {
        let rows = vec![
            ledger_row_at(
                "V20260101000000__early1",
                "billing",
                true,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260201000000__early2",
                "billing",
                true,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260301000000__later",
                "billing",
                false,
                LedgerStatus::Applied,
            ),
        ];
        let policy = crate::config::PolicyConfig::default();
        let mut diagnostics = Vec::new();
        emit_out_of_order_diagnostics(&rows, &policy, &mut diagnostics);
        let d622: Vec<&VerifyDiagnostic> =
            diagnostics.iter().filter(|d| d.code == "D622").collect();
        assert_eq!(d622.len(), 2, "one D622 per ooo row");
    }

    #[test]
    fn d622_ignores_rolled_back_peer() {
        // A rolled-back peer is not a conflicting peer (the operator
        // explicitly reverted it). The OOO row's diagnostic still
        // emits, but with a "no peer found" message.
        let rows = vec![
            ledger_row_at(
                "V20260201000000__early",
                "billing",
                true,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260301000000__rolled",
                "billing",
                false,
                LedgerStatus::RolledBack,
            ),
        ];
        let policy = crate::config::PolicyConfig::default();
        let mut diagnostics = Vec::new();
        emit_out_of_order_diagnostics(&rows, &policy, &mut diagnostics);
        let d622: Vec<&VerifyDiagnostic> =
            diagnostics.iter().filter(|d| d.code == "D622").collect();
        assert_eq!(d622.len(), 1);
        assert!(
            d622[0].message.contains("no higher-version peer"),
            "expected a `no higher-version peer` note: {}",
            d622[0].message
        );
    }

    #[test]
    fn d622_scopes_peer_lookup_to_same_app_label() {
        // An out-of-order row in `billing` must not pull in a peer
        // from `users` even if the peer's version is lexically higher.
        let rows = vec![
            ledger_row_at(
                "V20260201000000__early",
                "billing",
                true,
                LedgerStatus::Applied,
            ),
            ledger_row_at(
                "V20260301000000__other_app_peer",
                "users",
                false,
                LedgerStatus::Applied,
            ),
        ];
        let policy = crate::config::PolicyConfig::default();
        let mut diagnostics = Vec::new();
        emit_out_of_order_diagnostics(&rows, &policy, &mut diagnostics);
        let d622: Vec<&VerifyDiagnostic> =
            diagnostics.iter().filter(|d| d.code == "D622").collect();
        assert_eq!(d622.len(), 1);
        assert!(
            d622[0].message.contains("no higher-version peer"),
            "no peer should match across app labels: {}",
            d622[0].message
        );
    }

    // ── D6xx code-uniqueness audit (A-2) ─────────────────────────────────

    /// Master table of every D6xx diagnostic code that can be emitted
    /// from this module. The audit test below walks the verify.rs
    /// source at compile time and asserts every emitted code literal
    /// appears here (and that each entry is unique). Adding a new
    /// emit site without updating this table is a hard test failure.
    ///
    /// Codex round-2 A-2: the previous test inspected this hand-typed
    /// array directly, which left a hole — a new code emitted in the
    /// module body but absent from the array escaped the uniqueness
    /// check. The audit test now closes that hole by cross-checking
    /// the table against the source file's emit-site literals.
    const D6XX_CODE_REGISTRY: &[(&str, &str)] = &[
        ("D601", "snapshot table missing in live"),
        ("D602", "live table not in snapshot"),
        ("D603", "snapshot column missing in live"),
        ("D604", "live column not in snapshot"),
        ("D605", "nullability drift"),
        ("D606", "type-string drift (advisory)"),
        ("D607", "default value drift"),
        ("D608", "primary key column list drift"),
        ("D609", "foreign-key shape drift"),
        ("D610", "snapshot index missing in live"),
        ("D611", "live index not in snapshot (advisory)"),
        ("D612", "index columns differ"),
        ("D613", "index uniqueness differs"),
        ("D614", "index method drift (advisory)"),
        ("D615", "index lives on wrong table"),
        ("D621", "ledger table not found"),
        ("D622", "out-of-order migration applied"),
        ("D690", "FTS not yet checked (info)"),
        ("D691", "partition not yet checked (info)"),
        ("D692", "enums not yet checked (info)"),
        ("D693", "INCLUDE / partial-predicate not yet checked (info)"),
        ("D699", "ledger reports applied but DB has no tables"),
    ];

    #[test]
    fn d6xx_codes_have_unique_meanings() {
        // Pin the current code -> meaning mapping. If a new diagnostic
        // is added that re-uses an existing code, this test should be
        // updated (and the duplicate refused via review).
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for (code, meaning) in D6XX_CODE_REGISTRY {
            assert!(
                seen.insert(code, meaning).is_none(),
                "duplicate D6xx code {code}",
            );
        }
    }

    #[test]
    fn d6xx_emit_sites_all_covered_by_registry() {
        // Codex round-2 A-2: walk the verify.rs source for every
        // VerifyDiagnostic emit site's code literal and assert each
        // one appears in the master registry above. A new emit site
        // that adds a code without listing it in
        // `D6XX_CODE_REGISTRY` fails this test.
        //
        // Implementation: `include_str!` pulls the source text in at
        // compile time. We forward-scan for the canonical emit-site
        // byte sequence — the field-assignment `code` followed by
        // colon-space-quote-D — and read the four-character code that
        // follows. A byte-level scan keeps us inside the no-regex rule.
        //
        // False-positive guard. The prefix is composed at runtime
        // from a plain string and a uppercase-D byte so the prefix
        // bytes do not appear verbatim in this very test's source —
        // otherwise the scanner would match its own scan-target
        // string. Comment and docstring prose elsewhere in this file
        // never carry the exact eight-byte sequence consisting of
        // the four letters `c`, `o`, `d`, `e`, then a colon, then a
        // space, then a double-quote, then an uppercase letter `D`,
        // because the verify.rs prose consistently writes the codes
        // as plain identifiers (`D601`, `D621`) without the field-
        // assignment form. Registry entries are formatted as
        // `("D6...", "...")` — preceded by `(` rather than by the
        // field-assignment prefix — so the registry itself does not
        // match the emit-site prefix.
        let source = include_str!("verify.rs");
        let bytes = source.as_bytes();
        // Compose the prefix at runtime from two halves so the
        // verbatim bytes never appear in this source file. This
        // prevents the scanner from matching its own scan-target
        // when it walks itself.
        let mut prefix_buf = Vec::with_capacity(8);
        prefix_buf.extend_from_slice(b"code: ");
        prefix_buf.push(b'"');
        prefix_buf.push(b'D');
        let prefix: &[u8] = &prefix_buf;
        let mut scan = 0usize;
        let mut emitted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        while scan + prefix.len() + 3 <= bytes.len() {
            if &bytes[scan..scan + prefix.len()] == prefix {
                let code_start = scan + prefix.len() - 1; // points at 'D'
                let code_end = code_start + 4;
                if code_end < bytes.len() && bytes[code_end] == b'"' {
                    // Validate the next three bytes are ASCII alphanumerics
                    // (the D6xx codes are uppercase 'D' followed by three
                    // ASCII digits / letters in practice).
                    let body_ok = bytes[code_start + 1..code_end]
                        .iter()
                        .all(|b| b.is_ascii_alphanumeric());
                    if body_ok {
                        let s = String::from_utf8_lossy(&bytes[code_start..code_end]).into_owned();
                        emitted.insert(s);
                    }
                }
                scan += prefix.len();
                continue;
            }
            scan += 1;
        }
        assert!(
            !emitted.is_empty(),
            "scanner found zero D6xx emit sites in verify.rs — \
             the byte pattern probably drifted; investigate before \
             trusting this test",
        );
        let registry: std::collections::BTreeSet<&str> =
            D6XX_CODE_REGISTRY.iter().map(|(c, _)| *c).collect();
        for code in &emitted {
            assert!(
                registry.contains(code.as_str()),
                "emit site for {code} found in verify.rs but {code} is \
                 missing from D6XX_CODE_REGISTRY — add it (with a \
                 unique meaning) to keep the central registry honest",
            );
        }
        // Reverse direction: every registry entry should have at
        // least one emit site. (A registry-only entry is fine in
        // principle — e.g. a code reserved for a future emit site —
        // but in practice every current entry SHOULD be emitted, so
        // we surface the gap as a soft `eprintln!` rather than a
        // hard assertion to keep the test focused on the duplicate-
        // and-missing failure mode.)
        for (code, _) in D6XX_CODE_REGISTRY {
            if !emitted.contains(*code) {
                eprintln!(
                    "note: D6xx code {code} is in the registry but has \
                     no emit site in verify.rs — either remove it from \
                     the registry or add the corresponding emit site"
                );
            }
        }
    }
}
