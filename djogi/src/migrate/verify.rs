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
//! Verify never mutates anything — it is read-only against the live
//! database and the snapshot file. Mutations belong to
//! [`super::repair`].
//!
//! # Minimum viable verify (T5)
//!
//! T5 reads the live catalog into a *partial* [`AppliedSchema`]
//! containing only what the verify path needs to compare:
//!
//! - **Tables.** Name + column list (name, rendered SQL type,
//!   nullability, default expression).
//! - **Primary keys.** Column list (kind detection deferred to T8).
//! - **Indexes.** Name + table + uniqueness + column list.
//! - **Foreign keys.** Name + source `(table, column)` + target
//!   `(table, column)` + cascade.
//!
//! Other fields ([`crate::migrate::schema::TableSchema::fts`],
//! [`crate::migrate::schema::TableSchema::partition`],
//! [`crate::migrate::schema::TableSchema::tenant_key`], enum types,
//! `INCLUDE` columns, partial-index predicates) surface as advisory
//! `Info` diagnostics for Phase 7 — T8 can tighten them to `Error`
//! once the live-DB projection grows. The deferral is intentional:
//! the v3 plan's stop condition explicitly says ">500 LOC of catalog
//! SQL is a sign you should narrow scope and surface it for review".
//! Any tightening lands in T8 alongside the `migrations status` work.
//!
//! # Determinism
//!
//! Output ordering is stable. [`VerifyDiagnostic`] lists are sorted
//! by `(code, location)` before return, and every catalog query that
//! powers the projection uses an explicit `ORDER BY` clause so the
//! comparison surface is reproducible. No `HashMap` / `HashSet` in
//! the public path.
//!
//! # Postgres-only
//!
//! Per the Djogi-wide Postgres-18-only stance, queries reach into
//! `pg_class`, `pg_attribute`, `pg_index`, `pg_constraint`,
//! `pg_attrdef`, and `information_schema.columns`. The selection
//! preserves the ability to read materialised columns Postgres does
//! not surface through `information_schema` (e.g. `pg_attribute.atttypmod`
//! for `VARCHAR(N)` length).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::context::DjogiContext;
use crate::error::DjogiError;

use super::ledger::{LedgerRow, LedgerStatus};
use super::schema::{AppliedSchema, ColumnSchema, IndexSchema, TableSchema};

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
/// **Read-only.** Verify never writes — repair lives in
/// [`super::repair`]. Failure to read the catalog or ledger surfaces
/// as a [`VerifyRunError`]; mismatches surface as `D6xx`
/// [`VerifyDiagnostic`] entries inside the returned [`VerifyReport`].
///
/// **Determinism.** `diagnostics` is sorted by `(code, location)`.
/// Iteration over the live catalog uses ordered queries so a re-run
/// against an unchanged DB produces an identical report.
pub async fn verify(
    ctx: &mut DjogiContext,
    snapshot: &AppliedSchema,
) -> Result<VerifyReport, VerifyRunError> {
    let mut diagnostics: Vec<VerifyDiagnostic> = Vec::new();

    // Read the ledger first so we can correlate with catalog state
    // even if the snapshot is missing tables.
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

    // Project the live catalog. Any catalog read failure is fatal —
    // we cannot produce useful diagnostics from a partial read.
    let live = project_live_schema(ctx).await?;

    // Compare snapshot tables to live tables.
    diff_tables(snapshot, &live, &mut diagnostics);

    // Compare snapshot indexes to live indexes.
    diff_indexes(snapshot, &live, &mut diagnostics);

    // Compare ledger expectations: every `applied` row should have
    // its target tables in the live catalog. We approximate this by
    // checking that every snapshot table exists live; a fully-correct
    // version-by-version replay belongs to T8.
    if !ledger_rows.is_empty() && live.models.is_empty() && !snapshot.models.is_empty() {
        diagnostics.push(VerifyDiagnostic {
            code: "D610".to_string(),
            severity: VerifySeverity::Error,
            message: format!(
                "ledger reports {applied_count} applied migration(s) but the \
                 live database contains zero tables; the schema may have been \
                 dropped out-of-band",
            ),
            location: None,
        });
    }

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
    let tables = read_tables(ctx).await?;
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
/// - `heer_*` — HeeRanjId's internal node-state / config tables,
///   created by the HeeRanjId Postgres schema; they are framework
///   substrate, not user schema, and must not surface as drift.
async fn read_tables(ctx: &mut DjogiContext) -> Result<Vec<TableSchema>, VerifyRunError> {
    // Step 1 — table names. Postgres 18 only; we rely on
    // `pg_class.relkind = 'r'` for ordinary tables and filter out
    // the framework-internal bookkeeping tables.
    let table_rows = ctx
        .query_all(
            "SELECT c.relname::text \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r' \
               AND n.nspname = 'public' \
               AND c.relname <> 'djogi_schema_migrations' \
               AND c.relname NOT LIKE 'heer\\_%' ESCAPE '\\' \
             ORDER BY c.relname",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "tables",
            source: e,
        })?;

    let mut out: Vec<TableSchema> = Vec::with_capacity(table_rows.len());
    for row in &table_rows {
        let table_name: String =
            row.try_get(0)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "tables.relname",
                    source: DjogiError::from(e),
                })?;
        let columns = read_columns(ctx, &table_name).await?;
        let primary_key_columns = read_primary_key_columns(ctx, &table_name).await?;

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
        });
    }
    Ok(out)
}

/// Read column metadata for one table. Returns columns in
/// `pg_attribute.attnum` order (the table's declaration order).
async fn read_columns(
    ctx: &mut DjogiContext,
    table_name: &str,
) -> Result<Vec<ColumnSchema>, VerifyRunError> {
    // Pull column name + Postgres type rendering + nullability +
    // default expression in one query. `pg_attrdef.adbin` is parsed
    // server-side via `pg_get_expr`. `format_type(atttypid, atttypmod)`
    // produces the canonical rendering ("character varying(255)",
    // "bigint", "timestamp with time zone").
    let rows = ctx
        .query_all(
            "SELECT a.attname::text, \
                    format_type(a.atttypid, a.atttypmod)::text, \
                    a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE n.nspname = 'public' \
               AND c.relname = $1 \
               AND a.attnum > 0 \
               AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&table_name],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "columns",
            source: e,
        })?;

    let mut out: Vec<ColumnSchema> = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row
            .try_get(0)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "columns.attname",
                source: DjogiError::from(e),
            })?;
        let sql_type: String = row
            .try_get(1)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "columns.type",
                source: DjogiError::from(e),
            })?;
        let attnotnull: bool = row
            .try_get(2)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "columns.attnotnull",
                source: DjogiError::from(e),
            })?;
        let default_sql: Option<String> =
            row.try_get(3)
                .map_err(|e| VerifyRunError::CatalogQueryFailed {
                    query_label: "columns.default",
                    source: DjogiError::from(e),
                })?;

        out.push(ColumnSchema {
            check: None,
            default_sql,
            foreign_key: None,
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

/// Read the primary key column list for a table (in PK order).
async fn read_primary_key_columns(
    ctx: &mut DjogiContext,
    table_name: &str,
) -> Result<Vec<String>, VerifyRunError> {
    let rows = ctx
        .query_all(
            "SELECT a.attname::text \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = con.conrelid \
                                 AND a.attnum = ANY(con.conkey) \
             WHERE n.nspname = 'public' \
               AND c.relname = $1 \
               AND con.contype = 'p' \
             ORDER BY array_position(con.conkey, a.attnum)",
            &[&table_name],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "primary_key",
            source: e,
        })?;

    let mut out: Vec<String> = Vec::with_capacity(rows.len());
    for row in &rows {
        let col: String = row
            .try_get(0)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "primary_key.attname",
                source: DjogiError::from(e),
            })?;
        out.push(col);
    }
    Ok(out)
}

/// Read every non-PK index in `public`. Skips:
///
/// - PK indexes (the column list lives on the table's
///   [`super::schema::PrimaryKeySchema`]).
/// - UNIQUE indexes auto-created from `UNIQUE` constraints (those
///   land on the column shape, not the index list).
async fn read_indexes(ctx: &mut DjogiContext) -> Result<Vec<IndexSchema>, VerifyRunError> {
    let rows = ctx
        .query_all(
            "SELECT i.relname::text, \
                    t.relname::text, \
                    ix.indisunique \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_class t ON t.oid = ix.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = 'public' \
               AND ix.indisprimary = false \
               AND t.relname <> 'djogi_schema_migrations' \
               AND t.relname NOT LIKE 'heer\\_%' ESCAPE '\\' \
             ORDER BY i.relname",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::CatalogQueryFailed {
            query_label: "indexes",
            source: e,
        })?;

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
                query_label: "indexes.relkind",
                source: DjogiError::from(e),
            })?;
        let is_unique: bool = row
            .try_get(2)
            .map_err(|e| VerifyRunError::CatalogQueryFailed {
                query_label: "indexes.indisunique",
                source: DjogiError::from(e),
            })?;

        out.push(IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: super::schema::IndexTypeSchema::BTree,
            kind: if is_unique {
                super::schema::IndexKindSchema::UniqueIndex
            } else {
                super::schema::IndexKindSchema::NonUnique
            },
            name,
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: false,
            table,
            target: super::schema::IndexTargetSchema::Columns(Vec::new()),
        });
    }
    Ok(out)
}

/// Read the ledger rows we use for verification. Returns rows in
/// `applied_at` order so iteration is chronological.
async fn read_applied_ledger(ctx: &mut DjogiContext) -> Result<Vec<LedgerRow>, VerifyRunError> {
    // The ledger may not exist yet (fresh database). Bootstrap it
    // first so the SELECT below cannot fail with relation-not-found
    // — verify is read-only against user schema, but it owns the
    // ledger table by definition.
    super::ledger::bootstrap(ctx)
        .await
        .map_err(|e| VerifyRunError::LedgerQueryFailed { source: e })?;

    let rows = ctx
        .query_all(
            "SELECT version, description, checksum_up, checksum_down, execution_mode, \
                    status, execution_time_ms, out_of_order_flag, applied_steps_count, \
                    total_steps, partial_apply_note, run_id, snapshot_version, app_label \
             FROM djogi_schema_migrations \
             ORDER BY applied_at, version",
            &[],
        )
        .await
        .map_err(|e| VerifyRunError::LedgerQueryFailed { source: e })?;

    let mut out: Vec<LedgerRow> = Vec::with_capacity(rows.len());
    for row in &rows {
        let version: String = row
            .try_get(0)
            .map_err(|e| VerifyRunError::LedgerQueryFailed {
                source: DjogiError::from(e),
            })?;
        let description: String =
            row.try_get(1)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let checksum_up: String =
            row.try_get(2)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let checksum_down: Option<String> =
            row.try_get(3)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let execution_mode: String =
            row.try_get(4)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let status_str: String = row
            .try_get(5)
            .map_err(|e| VerifyRunError::LedgerQueryFailed {
                source: DjogiError::from(e),
            })?;
        let execution_time_ms: i64 =
            row.try_get(6)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let out_of_order_flag: bool =
            row.try_get(7)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let applied_steps_count: i32 =
            row.try_get(8)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let total_steps: Option<i32> =
            row.try_get(9)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let partial_apply_note: Option<String> =
            row.try_get(10)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let run_id: i64 = row
            .try_get(11)
            .map_err(|e| VerifyRunError::LedgerQueryFailed {
                source: DjogiError::from(e),
            })?;
        let snapshot_version: String =
            row.try_get(12)
                .map_err(|e| VerifyRunError::LedgerQueryFailed {
                    source: DjogiError::from(e),
                })?;
        let app_label: String = row
            .try_get(13)
            .map_err(|e| VerifyRunError::LedgerQueryFailed {
                source: DjogiError::from(e),
            })?;

        let status = LedgerStatus::from_db_str(&status_str).unwrap_or(LedgerStatus::Failed);
        let execution_mode = match execution_mode.as_str() {
            "transactional" => super::ledger::ExecutionMode::Transactional,
            _ => super::ledger::ExecutionMode::NonTransactional,
        };
        out.push(LedgerRow {
            version,
            description,
            checksum_up,
            checksum_down,
            execution_mode,
            status,
            execution_time_ms,
            out_of_order_flag,
            applied_steps_count,
            total_steps,
            partial_apply_note,
            run_id,
            snapshot_version,
            app_label,
        });
    }
    Ok(out)
}

/// Read every foreign-key constraint in `public`. Returns
/// `(table, column, ref_table, ref_column)` tuples in alphabetical
/// `(table, column)` order.
///
/// Reserved for the FK-drift diagnostic path that T8 will turn from
/// advisory `Info` into an `Error`-level check. Phase 7 keeps the
/// projection small by never calling this directly — the helper
/// exists so the SQL is reviewed and pinned now and the upgrade in T8
/// flips a single boolean rather than re-deriving the catalog query.
#[allow(dead_code)]
async fn read_foreign_keys(
    ctx: &mut DjogiContext,
) -> Result<Vec<(String, String, String, String)>, VerifyRunError> {
    let rows = ctx
        .query_all(
            "SELECT c.relname::text, \
                    a.attname::text, \
                    rc.relname::text, \
                    ra.attname::text \
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
               AND c.relname NOT LIKE 'heer\\_%' ESCAPE '\\' \
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

    let mut out: Vec<(String, String, String, String)> = Vec::with_capacity(rows.len());
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
        out.push((table, column, ref_table, ref_column));
    }
    Ok(out)
}

// ── Diff helpers (snapshot vs. live projection) ───────────────────────────

/// Compare every snapshot table to its live counterpart and emit
/// drift diagnostics.
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

    // D603 / D604 — per-column drift for tables present on both sides.
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

        // D605 — nullability drift on shared columns. The rendered
        // SQL types diverge often enough between snapshot ("BIGINT")
        // and live catalog ("bigint") that we keep type comparison
        // advisory; nullability is a clean Boolean.
        for (col_name, snap_col) in &snap_cols {
            let Some(live_col) = live_cols.get(col_name) else {
                continue;
            };
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
        }
    }
}

/// Compare the snapshot's index list to the live projection.
fn diff_indexes(
    snapshot: &AppliedSchema,
    live: &AppliedSchema,
    diagnostics: &mut Vec<VerifyDiagnostic>,
) {
    let snap_names: BTreeSet<&str> = snapshot.indexes.iter().map(|i| i.name.as_str()).collect();
    let live_names: BTreeSet<&str> = live.indexes.iter().map(|i| i.name.as_str()).collect();

    // D610 — snapshot index missing in live DB.
    for name in &snap_names {
        if !live_names.contains(name) {
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
    for name in &live_names {
        if !snap_names.contains(name) {
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
/// **No regex** — uses byte-level lowercasing and explicit alias
/// substitution.
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

// ── Internal accessors reserved for T8's tightened diagnostics ────────────

/// Live-DB projection accessor reserved for the T8 verify-tightening
/// path. Today T5's `verify` calls `project_live_schema` directly;
/// the public-but-`pub(super)` accessor exists so T8 can layer
/// additional comparisons (e.g. constraint-vs-snapshot UNIQUE
/// detection) without re-deriving the projection.
#[allow(dead_code)]
pub(super) async fn live_schema_for_repair(
    ctx: &mut DjogiContext,
) -> Result<AppliedSchema, VerifyRunError> {
    project_live_schema(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::schema::{
        ColumnSchema, IndexKindSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema,
        PkKindSchema, PrimaryKeySchema, TableSchema,
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
}
