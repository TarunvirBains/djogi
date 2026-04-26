//! `djogi migrations attune` — local-history reconciliation.
//!
//! # Scope (Phase 7 v3 §8 / T7 — OQ-04 amendment)
//!
//! `attune` operates on local migration history. Three modes:
//!
//! 1. **Default (`AttuneMode::DiffOnly`)**: read-only diff between
//!    on-disk SQL files and the ledger. Reports SQL files present on
//!    disk but absent from the ledger ("unrecorded"), and ledger rows
//!    whose corresponding SQL file is missing on disk ("orphaned").
//!    Acquires the workspace file lock to take a consistent snapshot
//!    but never writes.
//!
//! 2. **`AttuneMode::Record`**: walks the same drift report and
//!    INSERTs ledger rows for every unrecorded SQL file, with
//!    `status = 'applied'` and a `partial_apply_note` recording the
//!    operator-supplied reason. **Does NOT execute the SQL** — `Record`
//!    is the operator asserting "these migrations were already applied
//!    out-of-band". Distinct from `fake_apply_plan` because `Record`
//!    walks every unrecorded SQL file in the bucket in one go.
//!
//! 3. **`AttuneMode::Squash { from }`**: HISTORY REWRITE. Coalesces
//!    every committed SQL file from `from` to HEAD into one squashed
//!    file, deletes the originals, and removes the deleted versions
//!    from the ledger. Per OQ-04 (Codex round-3 lens-auto-resolved):
//!
//!    - **Localhost-only.** Refuses to run when `DATABASE_URL` does
//!      not resolve to the local machine — see
//!      [`crate::migrate::policy::is_localhost_connection`]. A typo in
//!      the URL pointing at staging cannot rewrite history that other
//!      developers also pull from.
//!    - **Dev-profile-only.** Refuses to run when
//!      `Djogi.toml::profile = "production"`. Production environments
//!      have a hard line against destructive history rewrites.
//!    - **Local-only by default.** The `--publish` flag must be
//!      explicitly passed for the squashed history to be pushed to
//!      the remote `migrations` submodule. Without it, the rewrite
//!      stays local — the operator can inspect the result, run the
//!      test suite, and only then publish.
//!
//! # File-lock contract
//!
//! Every mode acquires the workspace [`super::guard::WorkspaceGuard`]
//! before touching any path. Concurrent compose / apply / repair
//! invocations cannot interleave with attune.
//!
//! # No regex
//!
//! Per the Djogi-wide no-regex rule, the SQL filename detection uses
//! byte-level prefix / suffix checks against the [`super::naming`]
//! module's emitted shapes. A "version" is recovered via
//! [`super::naming::version_id`] / [`super::naming::version_prefix`]
//! by walking the leading `V` then digits / underscores prefix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::context::DjogiContext;
use crate::error::DjogiError;

use super::guard::WorkspaceGuard;
use super::ledger::{
    self, ExecutionMode, LedgerRow, LedgerStatus, SHA256_HEX_LEN, compute_checksum,
};
use super::naming::{down_filename, up_filename};
use super::projection::BucketKey;
use super::schema::SNAPSHOT_FORMAT_VERSION;
use super::target::{bucket_dir, scan_filesystem};

// ── Public types ──────────────────────────────────────────────────────────

/// Mode selector for [`attune`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttuneMode {
    /// Default — read-only diff between disk + ledger. Reports
    /// unrecorded files and orphaned ledger rows.
    DiffOnly,
    /// Insert ledger rows for SQL files present on disk but absent
    /// from the ledger. Records the operator-supplied `reason` in the
    /// row's `partial_apply_note`. Does NOT execute SQL.
    Record {
        /// Operator-supplied rationale; non-empty by convention. Per
        /// the audit trail, every recorded row carries this verbatim
        /// in `partial_apply_note`.
        reason: String,
    },
    /// HISTORY REWRITE. Squash every version from `from` (inclusive)
    /// to HEAD into a single migration whose `up` SQL is the
    /// concatenation of all subsumed up files (and `down` is the
    /// concatenation of subsumed downs in reverse). Deletes the
    /// originals from disk AND the ledger. Localhost + dev-profile
    /// gated.
    ///
    /// **`publish`** is the second gate: when `false`, the rewrite
    /// stays local. When `true`, attune shells out to
    /// `git -C <migrations_root> push` to publish. Default is
    /// `false`; a missing `--publish` flag NEVER auto-publishes.
    Squash {
        /// Inclusive starting version (e.g. `V20260101000000__init`).
        from: String,
        /// `true` to push to the migrations submodule's remote;
        /// `false` to keep the rewrite local.
        publish: bool,
    },
}

/// One row of a [`AttuneReport`] — describes a single drift item
/// between disk and ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttuneEntry {
    /// `(database, app)` bucket the entry belongs to.
    pub bucket: BucketKey,
    /// Migration version (e.g. `V20260425010203__add_users`). Always
    /// the recovered version key, never the file path.
    pub version: String,
    /// Drift kind — see [`AttuneEntryKind`].
    pub kind: AttuneEntryKind,
}

/// Kind of drift recorded by an [`AttuneEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttuneEntryKind {
    /// SQL file exists on disk but no ledger row matches its version.
    Unrecorded,
    /// Ledger row exists with status `applied` / `faked` / `baseline`
    /// but no SQL file with that version exists on disk.
    Orphaned,
    /// `Record` mode picked up an unrecorded entry and inserted a
    /// ledger row for it. Surfaced in the report so the CLI can
    /// distinguish "would record" (DiffOnly) from "did record"
    /// (Record).
    Recorded,
    /// `Squash` mode collapsed this version into the squash target.
    /// The originating SQL file was deleted and the ledger row removed.
    Squashed,
}

impl AttuneEntryKind {
    /// Stable lowercase string used by the CLI rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            AttuneEntryKind::Unrecorded => "unrecorded",
            AttuneEntryKind::Orphaned => "orphaned",
            AttuneEntryKind::Recorded => "recorded",
            AttuneEntryKind::Squashed => "squashed",
        }
    }
}

/// Result of [`attune`]. Sorted deterministically — the CLI prints
/// each entry in the order it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttuneReport {
    /// Drift entries — sorted by `(database, app, version, kind)`.
    pub entries: Vec<AttuneEntry>,
    /// `true` when the attune invocation made any database / disk
    /// mutation. `false` for a pure DiffOnly run.
    pub mutated: bool,
    /// Squash target version when [`AttuneMode::Squash`] succeeded.
    /// `None` for other modes.
    pub squashed_to: Option<String>,
    /// `true` when the squashed history was pushed to the remote.
    /// Only set in `Squash { publish: true }` after a successful push.
    pub published: bool,
}

/// Errors surfaced by [`attune`]. Distinct from [`super::runner::RunnerError`]
/// because attune does not run user SQL.
#[derive(Debug)]
pub enum AttuneError {
    /// Mode-specific gate refused before any work happened. Carries
    /// the precise reason so the operator-facing message is actionable.
    Refused(AttuneRefusal),
    /// I/O failure walking `migrations/`.
    FilesystemScanFailed { source: std::io::Error },
    /// Ledger query failed.
    LedgerQueryFailed { source: DjogiError },
    /// Failed to read a SQL file we needed to hash.
    SqlReadFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Failed to write the squashed SQL file.
    SqlWriteFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Failed to delete a SQL file during squash.
    SqlDeleteFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `git push` failed during `--publish`. Carries the captured
    /// stderr so the operator can diagnose without re-running.
    GitPublishFailed {
        stderr: String,
        status_code: Option<i32>,
    },
}

/// Specific refusal kind for [`AttuneError::Refused`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttuneRefusal {
    /// `Squash` mode was invoked but `DATABASE_URL` does not resolve
    /// to localhost. Surfaces the connection string verbatim so the
    /// operator can correct it (no secrets — DATABASE_URL is itself a
    /// secret-bearing string but the byte already reached this layer
    /// from the operator's environment).
    SquashNotLocalhost { database_url: String },
    /// `Squash` mode was invoked but `Djogi.toml::profile` is
    /// `"production"`.
    SquashNotDevProfile { profile: String },
    /// `Squash --from` named a version that does not exist on disk.
    /// We refuse rather than silently no-op because the version
    /// argument is load-bearing for an audit trail.
    SquashFromNotFound { from: String },
}

impl std::fmt::Display for AttuneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttuneError::Refused(r) => write!(f, "attune refused: {r}"),
            AttuneError::FilesystemScanFailed { source } => {
                write!(f, "attune filesystem scan failed: {source}")
            }
            AttuneError::LedgerQueryFailed { source } => {
                write!(f, "attune ledger query failed: {source}")
            }
            AttuneError::SqlReadFailed { path, source } => write!(
                f,
                "attune could not read SQL file at {}: {source}",
                path.display()
            ),
            AttuneError::SqlWriteFailed { path, source } => write!(
                f,
                "attune could not write squashed SQL at {}: {source}",
                path.display()
            ),
            AttuneError::SqlDeleteFailed { path, source } => write!(
                f,
                "attune could not delete subsumed SQL at {}: {source}",
                path.display()
            ),
            AttuneError::GitPublishFailed {
                stderr,
                status_code,
            } => match status_code {
                Some(c) => write!(
                    f,
                    "attune --publish: git push exited with status {c}: {stderr}"
                ),
                None => write!(
                    f,
                    "attune --publish: git push terminated by signal: {stderr}"
                ),
            },
        }
    }
}

impl std::fmt::Display for AttuneRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttuneRefusal::SquashNotLocalhost { database_url } => write!(
                f,
                "attune --squash refuses to run when DATABASE_URL is not localhost \
                 (got `{database_url}`); squash is a destructive history rewrite and \
                 must not be invoked against shared / production databases"
            ),
            AttuneRefusal::SquashNotDevProfile { profile } => write!(
                f,
                "attune --squash refuses to run with profile=`{profile}`; squash \
                 is dev-only — set `profile = \"development\"` (or remove the \
                 production override) before retrying"
            ),
            AttuneRefusal::SquashFromNotFound { from } => write!(
                f,
                "attune --squash --from `{from}` did not match any version on disk; \
                 list `migrations/<database>/<app>/` to find a valid starting version"
            ),
        }
    }
}

impl std::error::Error for AttuneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AttuneError::FilesystemScanFailed { source } => Some(source),
            AttuneError::LedgerQueryFailed { source } => Some(source),
            AttuneError::SqlReadFailed { source, .. } => Some(source),
            AttuneError::SqlWriteFailed { source, .. } => Some(source),
            AttuneError::SqlDeleteFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Configuration handed to [`attune`].
pub struct AttuneRequest<'a> {
    /// Workspace root — where `migrations/` lives.
    pub workspace_root: &'a Path,
    /// Connection string under examination. Used for the localhost
    /// gate when [`AttuneMode::Squash`] is requested. Never read for
    /// other modes.
    pub database_url: &'a str,
    /// Operator profile string from [`crate::config::DjogiConfig::profile`].
    /// Squash refuses on production.
    pub profile: &'a str,
    /// Mode selector — see [`AttuneMode`].
    pub mode: AttuneMode,
    /// Witness-typed proof that the workspace lock is held. Attune
    /// requires it for all three modes — even `DiffOnly` takes the
    /// lock so a concurrent compose / apply cannot mutate the tree
    /// mid-scan.
    pub _guard: &'a WorkspaceGuard,
}

// ── Public entry point ────────────────────────────────────────────────────

/// Run `attune` against the workspace.
///
/// **Witness-typed lock.** The `_guard: &WorkspaceGuard` parameter is
/// the same witness pattern as [`super::runner::apply_plan`]. The
/// caller proves it acquired the workspace lock before invoking
/// attune; the runner trusts the witness without re-acquiring.
///
/// **No SQL execution.** Attune never runs user DDL. `Record` mode
/// inserts ledger rows; `Squash` mode rewrites files + the ledger.
/// `DiffOnly` mode is read-only.
pub async fn attune(
    ctx: &mut DjogiContext,
    req: AttuneRequest<'_>,
) -> Result<AttuneReport, AttuneError> {
    // Squash mode runs all its gates BEFORE any I/O so a refusal
    // produces zero side effects.
    if let AttuneMode::Squash { .. } = &req.mode {
        if !super::policy::is_localhost_connection(req.database_url) {
            return Err(AttuneError::Refused(AttuneRefusal::SquashNotLocalhost {
                database_url: req.database_url.to_string(),
            }));
        }
        if req.profile == "production" {
            return Err(AttuneError::Refused(AttuneRefusal::SquashNotDevProfile {
                profile: req.profile.to_string(),
            }));
        }
    }

    // Bootstrap the ledger so the SELECT below cannot fail with
    // relation-not-found. This is a small write — the ledger DDL is
    // `CREATE TABLE IF NOT EXISTS`, idempotent — and is consistent
    // with the runner's own bootstrap pattern. Attune is gated on the
    // workspace lock so the bootstrap is serial against other
    // migration tooling.
    ledger::bootstrap(ctx)
        .await
        .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;

    // Walk disk + ledger.
    let disk = scan_disk(req.workspace_root)?;
    let ledger_versions = scan_ledger(ctx).await?;

    // Compute the diff: unrecorded (on disk, not in ledger) and
    // orphaned (in ledger, not on disk).
    let mut entries: Vec<AttuneEntry> = Vec::new();
    for (bucket, versions) in &disk {
        let ledger_for_bucket = ledger_versions.get(bucket).cloned().unwrap_or_default();
        for version in versions.keys() {
            if !ledger_for_bucket.contains_key(version) {
                entries.push(AttuneEntry {
                    bucket: bucket.clone(),
                    version: version.clone(),
                    kind: AttuneEntryKind::Unrecorded,
                });
            }
        }
    }
    for (bucket, versions) in &ledger_versions {
        let disk_for_bucket = disk.get(bucket).cloned().unwrap_or_default();
        for version in versions.keys() {
            if !disk_for_bucket.contains_key(version) {
                entries.push(AttuneEntry {
                    bucket: bucket.clone(),
                    version: version.clone(),
                    kind: AttuneEntryKind::Orphaned,
                });
            }
        }
    }

    let mut report = AttuneReport {
        entries: Vec::new(),
        mutated: false,
        squashed_to: None,
        published: false,
    };

    match &req.mode {
        AttuneMode::DiffOnly => {
            entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            report.entries = entries;
            Ok(report)
        }
        AttuneMode::Record { reason } => {
            // Reuse the diff entries but split: every Unrecorded gets
            // an INSERT + flips to Recorded; Orphaned passes through
            // unchanged.
            let mut out: Vec<AttuneEntry> = Vec::new();
            for entry in entries {
                match entry.kind {
                    AttuneEntryKind::Unrecorded => {
                        let path = disk
                            .get(&entry.bucket)
                            .and_then(|m| m.get(&entry.version))
                            .cloned();
                        if let Some(path) = path {
                            insert_recorded_row(ctx, &entry.bucket, &entry.version, &path, reason)
                                .await?;
                            report.mutated = true;
                            out.push(AttuneEntry {
                                kind: AttuneEntryKind::Recorded,
                                ..entry
                            });
                        } else {
                            out.push(entry);
                        }
                    }
                    _ => out.push(entry),
                }
            }
            out.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            report.entries = out;
            Ok(report)
        }
        AttuneMode::Squash { from, publish } => {
            run_squash(
                ctx,
                req.workspace_root,
                from,
                *publish,
                &disk,
                &mut report,
                entries,
            )
            .await?;
            Ok(report)
        }
    }
}

// ── Disk scan ─────────────────────────────────────────────────────────────

/// Walk `migrations/<database>/<app>/` and return every up-SQL file's
/// `(version → path)` map, keyed by bucket.
///
/// **Up files only.** A migration directory contains both up and down
/// SQL files; the up file is the canonical artifact (and its presence
/// is what `attune --record` recovers from). The down file is paired
/// 1:1 — a missing down for a present up surfaces in compose's
/// idempotency check, not here.
fn scan_disk(
    workspace_root: &Path,
) -> Result<BTreeMap<BucketKey, BTreeMap<String, PathBuf>>, AttuneError> {
    let mut out: BTreeMap<BucketKey, BTreeMap<String, PathBuf>> = BTreeMap::new();
    let buckets = scan_filesystem(workspace_root)
        .map_err(|e| AttuneError::FilesystemScanFailed { source: e })?;
    for fb in buckets {
        let bucket = BucketKey {
            database: fb.database,
            app: fb.app,
        };
        let dir = bucket_dir(workspace_root, &bucket);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(AttuneError::FilesystemScanFailed { source: err }),
        };
        for entry in entries {
            let entry = entry.map_err(|e| AttuneError::FilesystemScanFailed { source: e })?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            // Up files: `V<...>__<slug>.sql` and DO NOT contain
            // `.down.` (which is the down-file marker).
            if !name.starts_with('V') || !name.ends_with(".sql") {
                continue;
            }
            if name.contains(".down.") {
                continue;
            }
            // Strip the trailing `.sql` and recover the version
            // prefix. The version is the leading `V<digits>` portion.
            let stem = &name[..name.len() - 4];
            let Some(version) = recover_version_from_stem(stem) else {
                continue;
            };
            out.entry(bucket.clone())
                .or_default()
                .insert(version, entry.path());
        }
    }
    Ok(out)
}

/// Recover the canonical version ID from a filename stem (the part
/// before `.sql`). The stem looks like `V20260425010203__add_users`;
/// the version is `V20260425010203__add_users` itself when the slug
/// is canonical, but for tests / edge cases we accept the bare prefix
/// `V20260425010203` too.
///
/// Implementation: walk a `V` followed by ASCII digits, then optional
/// `__<slug>`. The leading prefix produced by [`version_prefix`] is
/// `V<14 ASCII digits>` so the deterministic case is straightforward.
fn recover_version_from_stem(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    if bytes.is_empty() || bytes[0] != b'V' {
        return None;
    }
    // Walk the digit body.
    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 {
        return None;
    }
    // Either we hit end-of-stem (bare-prefix form) or we hit `__` and
    // then the slug.
    if i == bytes.len() {
        return Some(stem.to_string());
    }
    if i + 1 < bytes.len() && bytes[i] == b'_' && bytes[i + 1] == b'_' {
        // The full version is `<prefix>__<slug>`. Return the whole
        // stem because the slug is part of the canonical version
        // identity.
        return Some(stem.to_string());
    }
    None
}

// ── Ledger scan ───────────────────────────────────────────────────────────

/// Read every ledger row's `version`, `app_label`, and `status`,
/// bucket-grouped. Only rows in a status that "asserts the migration
/// applied" (Applied, Faked, Baseline) are reported — `Pending`,
/// `Failed`, `RolledBack` do not count as orphaned-when-disk-missing
/// because the SQL file may still need to live alongside the row for
/// repair / re-apply.
async fn scan_ledger(
    ctx: &mut DjogiContext,
) -> Result<BTreeMap<BucketKey, BTreeMap<String, LedgerStatus>>, AttuneError> {
    let rows = ctx
        .query_all(
            "SELECT version, app_label, status \
             FROM djogi_schema_migrations \
             ORDER BY app_label, version",
            &[],
        )
        .await
        .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;

    // Attune is bucket-scoped at the (database, app) level, but the
    // ledger does not record `database` directly — it lives on the
    // bucket from which the runner derived the row. For T7 we treat
    // the active connection as a single database; the bucket
    // identity reduces to `(active_db, app_label)`. Attune's caller
    // is expected to invoke per-database (the orchestrator T7
    // lifecycle iterates buckets the same way `compose` does in T6).
    //
    // Today's runner is single-pool too; we mirror that. When
    // `DjogiContext::pool_for(database)` lands the call site picks
    // the right database name from the pool's metadata.
    let database = active_database_name(ctx).await?;

    let mut out: BTreeMap<BucketKey, BTreeMap<String, LedgerStatus>> = BTreeMap::new();
    for row in &rows {
        let version: String = row.try_get(0).map_err(|e| AttuneError::LedgerQueryFailed {
            source: DjogiError::from(e),
        })?;
        let app_label: String = row.try_get(1).map_err(|e| AttuneError::LedgerQueryFailed {
            source: DjogiError::from(e),
        })?;
        let status_s: String = row.try_get(2).map_err(|e| AttuneError::LedgerQueryFailed {
            source: DjogiError::from(e),
        })?;
        let status = LedgerStatus::from_db_str(&status_s).unwrap_or(LedgerStatus::Failed);
        if !matches!(
            status,
            LedgerStatus::Applied | LedgerStatus::Faked | LedgerStatus::Baseline
        ) {
            continue;
        }
        let bucket = BucketKey {
            database: database.clone(),
            app: app_label,
        };
        out.entry(bucket).or_default().insert(version, status);
    }
    Ok(out)
}

/// Read the active database's name from `current_database()`. Used to
/// stamp the bucket identity on ledger rows when reading them back.
/// The active database IS the bucket database for T7's single-pool
/// arrangement; the helper exists so a future multi-pool shape can
/// override the source without churning the call sites.
async fn active_database_name(ctx: &mut DjogiContext) -> Result<String, AttuneError> {
    let row = ctx
        .query_one("SELECT current_database()::text", &[])
        .await
        .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
    let name: String = row.try_get(0).map_err(|e| AttuneError::LedgerQueryFailed {
        source: DjogiError::from(e),
    })?;
    Ok(name)
}

// ── Record mode ───────────────────────────────────────────────────────────

/// Insert a `status='applied'` ledger row for an unrecorded SQL file.
/// The note carries the operator-supplied reason verbatim.
async fn insert_recorded_row(
    ctx: &mut DjogiContext,
    bucket: &BucketKey,
    version: &str,
    up_path: &Path,
    reason: &str,
) -> Result<(), AttuneError> {
    let up_sql = std::fs::read_to_string(up_path).map_err(|e| AttuneError::SqlReadFailed {
        path: up_path.to_path_buf(),
        source: e,
    })?;
    let checksum_up = compute_checksum([up_sql.as_str()]);
    // Try to read the down file too — the version's down checksum is
    // best-effort. Missing down is fine; record a None so the row
    // still inserts.
    let down_path = up_path
        .parent()
        .map(|p| p.join(super::naming::down_filename(version)));
    let checksum_down = down_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|down_sql| compute_checksum([down_sql.as_str()]));
    let _ = SHA256_HEX_LEN; // use the constant import for documentation; checksum_up enforces shape.

    let timestamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "<unknown>".to_string());
    let note = format!("attune --record at {timestamp}: {reason}");

    let row = LedgerRow {
        version: version.to_string(),
        description: format!("<attune --record> {version}"),
        checksum_up,
        checksum_down,
        execution_mode: ExecutionMode::Transactional,
        status: LedgerStatus::Applied,
        execution_time_ms: 0,
        out_of_order_flag: false,
        applied_steps_count: 0,
        total_steps: None,
        partial_apply_note: Some(note),
        run_id: 0, // Records are not tied to a runner invocation.
        snapshot_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        app_label: bucket.app.clone(),
    };
    let ledger_id = ledger::insert_pending(ctx, &row)
        .await
        .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
    // insert_pending writes status='pending' regardless of the row's
    // status field; flip explicitly.
    ctx.execute(
        "UPDATE djogi_schema_migrations SET status = 'applied' WHERE id = $1",
        &[&ledger_id],
    )
    .await
    .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
    let _ = down_filename(version); // silence unused-import lint when no down file exists
    let _ = up_filename(version);
    Ok(())
}

// ── Squash mode ───────────────────────────────────────────────────────────

/// Implement `AttuneMode::Squash`. Concatenates every up SQL file from
/// `from` to HEAD into a single squashed file, deletes the originals,
/// and removes the corresponding ledger rows. Optionally pushes to
/// the migrations submodule's remote when `publish = true`.
#[allow(clippy::too_many_arguments)]
async fn run_squash(
    ctx: &mut DjogiContext,
    workspace_root: &Path,
    from: &str,
    publish: bool,
    disk: &BTreeMap<BucketKey, BTreeMap<String, PathBuf>>,
    report: &mut AttuneReport,
    diff_entries: Vec<AttuneEntry>,
) -> Result<(), AttuneError> {
    // Surface any unmodified diff entries as part of the report — the
    // operator wants to see "what was already drifting" alongside the
    // squash.
    let mut entries = diff_entries;

    // Squash operates per-bucket. Validate that `from` matches a
    // version present somewhere on disk; reject early if not.
    let mut from_found = false;
    for versions in disk.values() {
        if versions.contains_key(from) {
            from_found = true;
            break;
        }
    }
    if !from_found {
        return Err(AttuneError::Refused(AttuneRefusal::SquashFromNotFound {
            from: from.to_string(),
        }));
    }

    for (bucket, versions) in disk {
        // Collect versions >= `from` in ascending order. Lexical
        // compare = chronological because version_prefix is
        // `V<14 digits>`.
        let to_squash: Vec<(&String, &PathBuf)> = versions
            .iter()
            .filter(|(v, _)| v.as_str() >= from)
            .collect();
        if to_squash.len() <= 1 {
            // Nothing to squash for this bucket — `from` is the only
            // version (or absent here entirely). Move on.
            continue;
        }
        let dir = bucket_dir(workspace_root, bucket);
        // Concatenate up SQL.
        let mut combined_up = String::new();
        let mut combined_down_segments: Vec<String> = Vec::new();
        for (version, path) in &to_squash {
            let up_sql = std::fs::read_to_string(path).map_err(|e| AttuneError::SqlReadFailed {
                path: (*path).clone(),
                source: e,
            })?;
            combined_up.push_str(&format!("-- begin {version}\n"));
            combined_up.push_str(&up_sql);
            if !up_sql.ends_with('\n') {
                combined_up.push('\n');
            }
            combined_up.push_str(&format!("-- end {version}\n\n"));
            // Down side — best-effort.
            let down_path = dir.join(down_filename(version));
            if let Ok(down_sql) = std::fs::read_to_string(&down_path) {
                combined_down_segments.push(format!(
                    "-- begin {version} (reverse)\n{down_sql}\n-- end {version}\n",
                ));
            }
        }
        // The squash target keeps `from` as its version label.
        let new_up_path = dir.join(up_filename(from));
        let new_down_path = dir.join(down_filename(from));
        std::fs::write(&new_up_path, combined_up.as_bytes()).map_err(|e| {
            AttuneError::SqlWriteFailed {
                path: new_up_path.clone(),
                source: e,
            }
        })?;
        // Down side: reverse-order concat of the per-version segments
        // collected above so a rollback unwinds in the same order
        // apply happened.
        let mut combined_down = String::new();
        for seg in combined_down_segments.iter().rev() {
            combined_down.push_str(seg);
            combined_down.push('\n');
        }
        if !combined_down.is_empty() {
            std::fs::write(&new_down_path, combined_down.as_bytes()).map_err(|e| {
                AttuneError::SqlWriteFailed {
                    path: new_down_path.clone(),
                    source: e,
                }
            })?;
        }
        // Delete the originals (everything in `to_squash` except `from`).
        for (version, path) in &to_squash {
            if version.as_str() == from {
                continue;
            }
            std::fs::remove_file(path).map_err(|e| AttuneError::SqlDeleteFailed {
                path: (*path).clone(),
                source: e,
            })?;
            let down = dir.join(down_filename(version));
            if down.exists() {
                std::fs::remove_file(&down).map_err(|e| AttuneError::SqlDeleteFailed {
                    path: down.clone(),
                    source: e,
                })?;
            }
            // Drop the ledger row.
            ctx.execute(
                "DELETE FROM djogi_schema_migrations WHERE version = $1 AND app_label = $2",
                &[version, &bucket.app],
            )
            .await
            .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
            entries.push(AttuneEntry {
                bucket: bucket.clone(),
                version: (*version).clone(),
                kind: AttuneEntryKind::Squashed,
            });
        }
        report.mutated = true;
        report.squashed_to = Some(from.to_string());
    }

    // Optional publish step.
    if publish && report.mutated {
        run_git_publish(workspace_root)?;
        report.published = true;
    }

    entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    report.entries = entries;
    Ok(())
}

/// Shell out to `git -C <migrations_root> push`. Captures stderr so
/// the operator can diagnose without re-running.
fn run_git_publish(workspace_root: &Path) -> Result<(), AttuneError> {
    let migrations_root = super::target::migrations_root(workspace_root);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("push")
        .output()
        .map_err(|e| AttuneError::GitPublishFailed {
            stderr: format!("failed to spawn git: {e}"),
            status_code: None,
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Err(AttuneError::GitPublishFailed {
        stderr,
        status_code: output.status.code(),
    })
}

// ── Sort key ──────────────────────────────────────────────────────────────

/// Stable sort key for [`AttuneEntry`]. The CLI prints entries in
/// `(database, app, version, kind)` order so two attune runs against
/// the same drift produce byte-identical output.
fn sort_key(e: &AttuneEntry) -> (String, String, String, &'static str) {
    (
        e.bucket.database.clone(),
        e.bucket.app.clone(),
        e.version.clone(),
        e.kind.as_str(),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("djogi-attune-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn recover_version_from_canonical_stem() {
        let v = recover_version_from_stem("V20260425010203__add_users").expect("canonical form");
        assert_eq!(v, "V20260425010203__add_users");
    }

    #[test]
    fn recover_version_from_bare_prefix() {
        let v = recover_version_from_stem("V20260425010203").expect("bare prefix");
        assert_eq!(v, "V20260425010203");
    }

    #[test]
    fn recover_version_rejects_no_v_prefix() {
        assert!(recover_version_from_stem("20260425010203__init").is_none());
    }

    #[test]
    fn recover_version_rejects_v_alone() {
        assert!(recover_version_from_stem("V").is_none());
    }

    #[test]
    fn recover_version_rejects_v_then_letters() {
        assert!(recover_version_from_stem("Vinit").is_none());
    }

    #[test]
    fn scan_disk_picks_up_only_up_files() {
        let root = temp_root("scan_disk_up_only");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        // Up file.
        fs::write(dir.join("V20260425010203__init.sql"), "CREATE TABLE foo();").unwrap();
        // Down file.
        fs::write(
            dir.join("V20260425010203__init.down.sql"),
            "DROP TABLE foo;",
        )
        .unwrap();
        // Random other file.
        fs::write(dir.join("README.md"), "noop").unwrap();
        let scanned = scan_disk(&root).expect("scan ok");
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let versions = scanned.get(&bucket).expect("billing bucket");
        assert_eq!(versions.len(), 1, "down + readme must be ignored");
        assert!(versions.contains_key("V20260425010203__init"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_disk_groups_by_bucket() {
        let root = temp_root("scan_disk_buckets");
        fs::create_dir_all(root.join("migrations/main/billing")).unwrap();
        fs::create_dir_all(root.join("migrations/main/_global_")).unwrap();
        fs::write(
            root.join("migrations/main/billing/V20260101000001__init.sql"),
            "",
        )
        .unwrap();
        fs::write(
            root.join("migrations/main/_global_/V20260101000002__init.sql"),
            "",
        )
        .unwrap();
        let scanned = scan_disk(&root).expect("scan ok");
        assert_eq!(scanned.len(), 2);
        let global = BucketKey {
            database: "main".to_string(),
            app: "".to_string(),
        };
        let billing = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        assert!(scanned.contains_key(&global), "global bucket present");
        assert!(scanned.contains_key(&billing), "billing bucket present");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sort_key_is_stable() {
        let a = AttuneEntry {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            version: "V20260101000001__init".to_string(),
            kind: AttuneEntryKind::Unrecorded,
        };
        let b = AttuneEntry {
            bucket: BucketKey {
                database: "main".to_string(),
                app: "billing".to_string(),
            },
            version: "V20260201000001__add".to_string(),
            kind: AttuneEntryKind::Unrecorded,
        };
        assert!(sort_key(&a) < sort_key(&b));
    }

    #[test]
    fn entry_kind_as_str_is_lowercase() {
        assert_eq!(AttuneEntryKind::Unrecorded.as_str(), "unrecorded");
        assert_eq!(AttuneEntryKind::Orphaned.as_str(), "orphaned");
        assert_eq!(AttuneEntryKind::Recorded.as_str(), "recorded");
        assert_eq!(AttuneEntryKind::Squashed.as_str(), "squashed");
    }

    #[test]
    fn refusal_displays_actionable_message() {
        let r = AttuneRefusal::SquashNotLocalhost {
            database_url: "postgres://prod.example.com/main".to_string(),
        };
        let s = format!("{r}");
        assert!(s.contains("postgres://prod.example.com/main"));
        assert!(s.contains("not localhost"));

        let r2 = AttuneRefusal::SquashNotDevProfile {
            profile: "production".to_string(),
        };
        let s2 = format!("{r2}");
        assert!(s2.contains("production"));
        assert!(s2.contains("dev-only"));
    }
}
