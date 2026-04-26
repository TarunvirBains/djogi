//! `migrations compose` orchestrator — T6's central entry point.
//!
//! Compose translates the descriptor inventory + the last-applied
//! snapshot into one new pair of files per drifted bucket:
//!
//! 1. The committed migration SQL pair under
//!    `migrations/<database>/<app>/<version>.sql` (up) +
//!    `<version>.down.sql` (down).
//! 2. The pending JSON at
//!    `target/djogi_pending/<database>/<app>.json` recording the
//!    composed delta + checksum (build.rs reads it as the second leg
//!    of the three-way match).
//!
//! The two writes are **atomic** — both succeed or neither. We write
//! to `<final>.tmp.<pid>` siblings, fsync, then rename the SQL pair
//! into place, then rename the pending JSON. On any rename failure
//! the partial state is rolled back.
//!
//! # OQ-08 — overwrite-on-same-slug
//!
//! Re-running `compose --name <slug>` against the same model state
//! and snapshot overwrites both files. The same input produces
//! byte-identical output (the SQL emitter is deterministic), so the
//! overwrite is a no-op on disk modulo the rename dance. Different
//! `--name` against the same delta refuses with [`ComposeError::NothingToCompose`]
//! because the differ produces an empty operation list.
//!
//! # OQ-10 / OQ-11 — lifecycle markers
//!
//! - `#[app(renamed_from = "old")]` → emit
//!   [`SchemaOperation::RenameApp`](super::diff::SchemaOperation::RenameApp)
//!   in addition to whatever the per-bucket diff produces, plus the
//!   folder-rename + ledger-UPDATE pair (per the v3 plan amendment).
//! - `#[app(tombstone)]` → require `--allow-destructive`; otherwise
//!   fail with [`ComposeError::TombstonedAppRequiresAllowDestructive`]
//!   carrying D011-shaped message text.
//! - `#[model(moved_from_app = OldApp)]` → emit
//!   [`SchemaOperation::MoveModelBetweenApps`](super::diff::SchemaOperation::MoveModelBetweenApps)
//!   (already handled by `diff_bucket_maps`).
//!
//! # No regex
//!
//! The slug derivation goes through [`super::naming::sanitize_slug`]
//! which is byte-level only.
//!
//! # `clippy::result_large_err`
//!
//! `ComposeError` carries the substantial `SqlEmitError` payload by
//! value to keep all of the diff-emitter context inspectable without
//! a heap hop. Every fallible function in this module returns the
//! same error type, so we silence the lint at module scope rather
//! than annotating each function. The migration crate's neighbour
//! modules (`sql.rs`, `segment.rs`, `projection.rs`) take the same
//! stance for their respective error types.

#![allow(clippy::result_large_err)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::diff::{Classification, SchemaDelta, SchemaOperation, diff_bucket_maps};
use super::guard::WorkspaceGuard;
use super::ledger::compute_checksum;
use super::naming::{down_filename, sanitize_slug, up_filename, version_id, version_prefix};
use super::projection::BucketKey;
use super::schema::AppliedSchema;
use super::segment::{MigrationPlan, plan_delta};
use super::snapshot::SnapshotError;
use super::sql::{OperationSql, lower_delta};
use super::target::{bucket_dir, pending_database_dir, pending_json_path};

// ── Errors ─────────────────────────────────────────────────────────────────

/// Errors surfaced by [`compose`].
#[derive(Debug)]
pub enum ComposeError {
    /// The differ produced an empty operation list for every bucket —
    /// nothing to compose. Distinct from a successful no-op so the
    /// caller can decide whether to print a friendly "all in sync"
    /// message vs. exit non-zero.
    NothingToCompose,
    /// `#[app(tombstone)]` was set on an app referenced in the
    /// compose, but the operator did not pass `--allow-destructive`.
    /// Carries D011-shaped diagnostic text.
    TombstonedAppRequiresAllowDestructive {
        /// App label that carries the tombstone marker.
        app_label: String,
        /// Database target the app belongs to.
        database: String,
        /// Pre-formatted diagnostic message — `D011: app "<app>" is
        /// tombstoned; pass --allow-destructive to emit the drop
        /// migration` (plus database context).
        text: String,
    },
    /// The composed delta carries `Classification::Destructive` /
    /// `Classification::Lossy` and the operator did not pass
    /// `--allow-destructive`. Distinct from the tombstone path
    /// because it covers ad-hoc drops outside lifecycle markers.
    DestructiveRequiresAllowDestructive {
        /// Affected bucket.
        bucket: BucketKey,
        /// Classification flavour (`Destructive` or `Lossy`).
        classification: Classification,
    },
    /// The differ produced [`Classification::Unsupported`] for at
    /// least one bucket — a non-flip PK transition, an enum variant
    /// removal, etc. The operator hand-writes the migration. T6 stops
    /// before any file is written.
    UnsupportedDelta { bucket: BucketKey, reason: String },
    /// SQL emission failed (e.g. a `PkTypeFlip` reached the standard
    /// path).
    SqlEmit(super::sql::SqlEmitError),
    /// Filesystem I/O — failed to create a directory, write a file,
    /// or rename one of the staged temp files.
    Io { path: PathBuf, source: io::Error },
    /// The pending JSON failed to serialize — should be unreachable
    /// for a well-formed `AppliedSchema` but surfaced for completeness.
    SerializeFailed(SnapshotError),
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingToCompose => write!(
                f,
                "D012: nothing to compose — model state matches snapshot for every bucket"
            ),
            Self::TombstonedAppRequiresAllowDestructive { text, .. } => f.write_str(text),
            Self::DestructiveRequiresAllowDestructive {
                bucket,
                classification,
            } => write!(
                f,
                "{database}/{app}: classification {classification:?} requires --allow-destructive",
                database = bucket.database,
                app = super::target::app_dirname(&bucket.app),
            ),
            Self::UnsupportedDelta { bucket, reason } => write!(
                f,
                "{database}/{app}: unsupported delta — {reason}",
                database = bucket.database,
                app = super::target::app_dirname(&bucket.app),
            ),
            Self::SqlEmit(e) => write!(f, "SQL emit failed: {e}"),
            Self::Io { path, source } => write!(f, "I/O at {}: {source}", path.display()),
            Self::SerializeFailed(e) => write!(f, "serialize failed: {e}"),
        }
    }
}

impl std::error::Error for ComposeError {}

// ── Inputs / outputs ───────────────────────────────────────────────────────

/// Inputs to [`compose`] — packaged into a struct so the entry point
/// stays a single positional argument (callers fill the struct with
/// either real-build values or test fixtures).
pub struct ComposeRequest<'a> {
    /// Workspace root. Migration tree lives at `<root>/migrations/`,
    /// pending JSON at `<root>/target/djogi_pending/`.
    pub workspace_root: &'a Path,
    /// The model state from the descriptor inventory, projected to
    /// per-bucket schemas. In production this is
    /// `project_from_inventory()`; tests pass a hand-rolled map.
    pub models: &'a std::collections::BTreeMap<BucketKey, AppliedSchema>,
    /// Per-bucket last-applied snapshots from disk. Buckets absent
    /// from this map are treated as having no prior schema (fresh
    /// app — every model is a new addition).
    pub snapshots: &'a std::collections::BTreeMap<BucketKey, AppliedSchema>,
    /// App-level lifecycle metadata — `renamed_from` / `tombstone` /
    /// `database` per registered app. Sourced from
    /// `AppRegistry::all()` in production.
    pub apps: &'a [AppLifecycle],
    /// Operator-supplied migration name (sanitised through
    /// [`sanitize_slug`]). Empty / missing produces the literal
    /// `migration`.
    pub name: &'a str,
    /// Operator opt-in for destructive / lossy / tombstone migrations.
    pub allow_destructive: bool,
    /// Compose-time clock, used as the version-prefix instant.
    /// Production callers pass `OffsetDateTime::now_utc()`; tests
    /// pin a deterministic value so the version ID is byte-stable.
    pub now: OffsetDateTime,
    /// Witness-typed file lock — compose mutates `<workspace>/migrations/`
    /// and `<workspace>/target/djogi_pending/`, both of which require
    /// the workspace lock per the v3 §6 file-lock contract.
    pub _guard: &'a WorkspaceGuard,
}

/// Successful-compose report. Returned per-bucket so the caller can
/// log structured progress.
#[derive(Debug, Clone)]
pub struct ComposeReport {
    /// One entry per bucket that received a compose. Empty when
    /// every bucket was already in sync (callers handle this via the
    /// [`ComposeError::NothingToCompose`] error path).
    pub composed_buckets: Vec<ComposedBucket>,
}

/// Per-bucket success record.
#[derive(Debug, Clone)]
pub struct ComposedBucket {
    /// Bucket the compose targeted.
    pub bucket: BucketKey,
    /// Final version ID — `V<ts>__<slug>`.
    pub version: String,
    /// Path written for the up SQL.
    pub up_sql_path: PathBuf,
    /// Path written for the down SQL.
    pub down_sql_path: PathBuf,
    /// Path written for the pending JSON.
    pub pending_json_path: PathBuf,
    /// Classification of the lowered delta — surfaces the
    /// destructive / lossy decision the operator opted into.
    pub classification: Classification,
}

/// App-level lifecycle metadata — flat shape of the `AppRegistry::all()`
/// fields that compose actually consumes. Decoupled so test fixtures
/// don't need to register a real app via the `djogi::apps!` macro.
#[derive(Debug, Clone)]
pub struct AppLifecycle {
    /// App label — `""` for the synthetic global bucket.
    pub label: String,
    /// Database target this app belongs to.
    pub database: String,
    /// Prior label, if any. When set, compose emits a
    /// [`SchemaOperation::RenameApp`] alongside the per-bucket diff.
    pub renamed_from: Option<String>,
    /// `true` if the app is tombstoned. Triggers the D011 path.
    pub tombstone: bool,
}

// ── Pending JSON shape ─────────────────────────────────────────────────────

/// The shape persisted at `target/djogi_pending/<database>/<app>.json`.
///
/// Serialised with `#[serde(deny_unknown_fields)]` so the build.rs
/// reader rejects future-shape pending files explicitly rather than
/// silently dropping unknown keys. Format-version handling lives at
/// the top level so older Djogi reading a newer pending file gets a
/// clear upgrade error rather than a generic `unknown field`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingPlan {
    /// Pending JSON format version. Currently always `"1"`.
    pub format_version: String,
    /// Bucket the pending plan applies to. Owned strings so the file
    /// round-trips through serde without lifetime gymnastics.
    pub bucket_database: String,
    /// Bucket app label.
    pub bucket_app: String,
    /// Version ID — `V<ts>__<slug>`.
    pub version: String,
    /// Compose-time slug (post-sanitisation).
    pub slug: String,
    /// Snapshot at the model state — embedded so build.rs can do the
    /// three-way match without re-projecting the inventory.
    pub model_snapshot: AppliedSchema,
    /// Up-side SQL checksum — `V1:<sha256-hex>`.
    pub checksum_up: String,
    /// Down-side SQL checksum — `None` when every operation's down is
    /// a SQL-comment placeholder (every drop is lossy → no real
    /// rollback).
    pub checksum_down: Option<String>,
    /// Compose timestamp (RFC 3339 UTC, second precision).
    pub composed_at: String,
}

/// Pending-JSON format version. Bumped when the [`PendingPlan`] shape
/// changes incompatibly.
pub const PENDING_FORMAT_VERSION: &str = "1";

// ── Public entry point ─────────────────────────────────────────────────────

/// Run compose against the supplied request.
///
/// **Atomic per bucket.** Each bucket's three writes (up SQL, down
/// SQL, pending JSON) succeed together or roll back together. Across
/// buckets the writes are sequential — a failure on bucket N leaves
/// buckets 0..N composed and N+1..end uncomposed. Operators rerun
/// compose to clear the partial state.
///
/// **Acquires no locks itself.** The `_guard` parameter is the
/// caller's witness that the workspace lock is held — see
/// [`WorkspaceGuard`].
///
/// **Determinism.** Two invocations with the same `models`,
/// `snapshots`, `apps`, `name`, `allow_destructive` AND the same
/// `now` produce byte-identical output. Production callers pass
/// `OffsetDateTime::now_utc()`; tests pin a fixed instant.
pub fn compose(req: ComposeRequest<'_>) -> Result<ComposeReport, ComposeError> {
    // 1. Collect tombstone violations BEFORE any work — fail loudly
    //    when an active model still references a tombstoned app.
    if !req.allow_destructive {
        for app in req.apps {
            if !app.tombstone {
                continue;
            }
            // The tombstoned app appearing in `models` (with at least
            // one model) means the operator forgot the
            // `--allow-destructive` opt-in. Surface D011.
            let bucket_for_app = BucketKey {
                database: app.database.clone(),
                app: app.label.clone(),
            };
            if req
                .models
                .get(&bucket_for_app)
                .is_some_and(|s| !s.models.is_empty())
            {
                let text = format!(
                    "D011: app \"{label}\" is tombstoned; pass --allow-destructive to emit the drop migration",
                    label = if app.label.is_empty() {
                        "_global_"
                    } else {
                        app.label.as_str()
                    }
                );
                return Err(ComposeError::TombstonedAppRequiresAllowDestructive {
                    app_label: app.label.clone(),
                    database: app.database.clone(),
                    text,
                });
            }
        }
    }

    // 2. Run the differ across the full bucket map.
    let mut deltas = diff_bucket_maps(req.snapshots, req.models);

    // 3. Layer in `RenameApp` ops driven by `AppRegistry`'s
    //    `renamed_from` field. The differ doesn't see this — it works
    //    purely on snapshots — so compose injects the op on the
    //    DESTINATION bucket (the post-rename label).
    for app in req.apps {
        let Some(prior_label) = app.renamed_from.as_deref() else {
            continue;
        };
        let dest_bucket = BucketKey {
            database: app.database.clone(),
            app: app.label.clone(),
        };
        // Find the destination delta — guaranteed by `diff_bucket_maps`
        // to include every bucket that exists in either `models` or
        // `snapshots`.
        if let Some(delta) = deltas.iter_mut().find(|d| d.bucket == dest_bucket) {
            delta.operations.insert(
                0,
                SchemaOperation::RenameApp {
                    from: prior_label.to_string(),
                    to: app.label.clone(),
                },
            );
        }
    }

    // 4. Filter to non-empty deltas. NoOp deltas have classification
    //    `NoOp` and an empty operations vec; skip them. Renamed-only
    //    deltas DO carry operations and survive the filter.
    let mut effective: Vec<SchemaDelta> = deltas
        .into_iter()
        .filter(|d| !d.operations.is_empty() || !matches!(d.classification, Classification::NoOp))
        .collect();

    if effective.is_empty() {
        return Err(ComposeError::NothingToCompose);
    }

    // 5. Re-classify deltas that gained injected RenameApp ops and
    //    apply the destructive / unsupported gates.
    for delta in &mut effective {
        // RenameApp ops re-classify via `classify` (not exposed) but
        // a metadata-only op classifies as `Reversible` per the
        // existing classifier rules; a delta that already had drops
        // remains Destructive. We do not need to re-derive — the
        // existing classification is already correct because the
        // injected ops are metadata-only and don't escalate severity.
        match &delta.classification {
            Classification::Unsupported { reason } => {
                return Err(ComposeError::UnsupportedDelta {
                    bucket: delta.bucket.clone(),
                    reason: reason.clone(),
                });
            }
            Classification::Destructive | Classification::Lossy if !req.allow_destructive => {
                return Err(ComposeError::DestructiveRequiresAllowDestructive {
                    bucket: delta.bucket.clone(),
                    classification: delta.classification.clone(),
                });
            }
            _ => {}
        }
    }

    // 6. Lower each delta to SQL pairs + plan, write all artifacts.
    let slug = sanitize_slug(req.name);
    let prefix = version_prefix(req.now);
    let version = version_id(&prefix, &slug);
    let composed_at = format_rfc3339_seconds(req.now);

    let mut composed_buckets: Vec<ComposedBucket> = Vec::with_capacity(effective.len());
    let mut written_paths: Vec<PathBuf> = Vec::new();
    let result: Result<(), ComposeError> = (|| {
        for delta in &effective {
            // Shouldn't fail — we validated classifications above.
            let lowered = lower_delta(delta).map_err(ComposeError::SqlEmit)?;
            let _plan: MigrationPlan = plan_delta(delta).map_err(ComposeError::SqlEmit)?;

            let model_snapshot = req
                .models
                .get(&delta.bucket)
                .cloned()
                .unwrap_or_else(|| empty_schema_for(&delta.bucket));

            let (checksum_up, checksum_down) = compute_checksums(&lowered);

            let pending = PendingPlan {
                format_version: PENDING_FORMAT_VERSION.to_string(),
                bucket_database: delta.bucket.database.clone(),
                bucket_app: delta.bucket.app.clone(),
                version: version.clone(),
                slug: slug.clone(),
                model_snapshot,
                checksum_up: checksum_up.clone(),
                checksum_down: checksum_down.clone(),
                composed_at: composed_at.clone(),
            };

            let up_path = bucket_dir(req.workspace_root, &delta.bucket).join(up_filename(&version));
            let down_path =
                bucket_dir(req.workspace_root, &delta.bucket).join(down_filename(&version));
            let pending_path = pending_json_path(req.workspace_root, &delta.bucket);

            let up_sql = compose_up_text(&version, delta, &lowered);
            let down_sql = compose_down_text(&version, delta, &lowered);
            let pending_bytes = serialize_pending(&pending)?;

            // Stage tmp siblings.
            ensure_parent(&up_path)?;
            ensure_parent(&pending_path)?;
            let up_tmp = atomic_write(&up_path, up_sql.as_bytes())?;
            let down_tmp_result = atomic_write(&down_path, down_sql.as_bytes());
            let down_tmp = match down_tmp_result {
                Ok(p) => p,
                Err(e) => {
                    let _ = fs::remove_file(&up_tmp);
                    return Err(e);
                }
            };
            let pending_tmp_result = atomic_write(&pending_path, &pending_bytes);
            let pending_tmp = match pending_tmp_result {
                Ok(p) => p,
                Err(e) => {
                    let _ = fs::remove_file(&up_tmp);
                    let _ = fs::remove_file(&down_tmp);
                    return Err(e);
                }
            };

            // Promote tmps. Order: up SQL, down SQL, pending JSON.
            // On the final-rename failure, the earlier renames are
            // already committed to disk; we surface the partial state
            // with an explicit error so the operator knows which file
            // to clean up.
            promote_tmp(&up_tmp, &up_path)?;
            written_paths.push(up_path.clone());
            if let Err(e) = promote_tmp(&down_tmp, &down_path) {
                let _ = fs::remove_file(&up_path);
                written_paths.pop();
                let _ = fs::remove_file(&pending_tmp);
                return Err(e);
            }
            written_paths.push(down_path.clone());
            if let Err(e) = promote_tmp(&pending_tmp, &pending_path) {
                let _ = fs::remove_file(&up_path);
                let _ = fs::remove_file(&down_path);
                written_paths.pop();
                written_paths.pop();
                return Err(e);
            }
            written_paths.push(pending_path.clone());

            composed_buckets.push(ComposedBucket {
                bucket: delta.bucket.clone(),
                version: version.clone(),
                up_sql_path: up_path,
                down_sql_path: down_path,
                pending_json_path: pending_path,
                classification: delta.classification.clone(),
            });
        }
        Ok(())
    })();

    if let Err(e) = result {
        // Roll back any successfully-written files.
        for p in &written_paths {
            let _ = fs::remove_file(p);
        }
        return Err(e);
    }

    Ok(ComposeReport { composed_buckets })
}

/// Format the [`PendingPlan`] as pretty-printed JSON with a trailing
/// newline.
fn serialize_pending(p: &PendingPlan) -> Result<Vec<u8>, ComposeError> {
    let mut bytes = serde_json::to_vec_pretty(p).map_err(|e| {
        ComposeError::SerializeFailed(SnapshotError::Parse {
            path: None,
            source: e,
        })
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn empty_schema_for(bucket: &BucketKey) -> AppliedSchema {
    AppliedSchema {
        djogi_version: env!("CARGO_PKG_VERSION").to_string(),
        enums: Default::default(),
        format_version: super::schema::SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: format_rfc3339_seconds(OffsetDateTime::UNIX_EPOCH),
        indexes: Vec::new(),
        models: Default::default(),
        registered_apps: vec![bucket.app.clone()],
    }
}

fn compute_checksums(lowered: &[OperationSql]) -> (String, Option<String>) {
    let up = compute_checksum(lowered.iter().map(|o| o.up.as_str()));
    let any_real_down = lowered.iter().any(|o| !o.down.starts_with("--"));
    let down = if any_real_down {
        Some(compute_checksum(lowered.iter().map(|o| o.down.as_str())))
    } else {
        None
    };
    (up, down)
}

/// Render the up-side SQL file. One header comment block followed by
/// each operation's SQL, separated by blank lines.
fn compose_up_text(version: &str, delta: &SchemaDelta, lowered: &[OperationSql]) -> String {
    let mut out = String::with_capacity(lowered.iter().map(|o| o.up.len()).sum::<usize>() + 256);
    out.push_str("-- Djogi composed migration — up\n");
    out.push_str(&format!("-- Version: {version}\n"));
    out.push_str(&format!(
        "-- Bucket:  {database}/{app}\n",
        database = delta.bucket.database,
        app = super::target::app_dirname(&delta.bucket.app),
    ));
    out.push_str(&format!(
        "-- Classification: {classification:?}\n",
        classification = delta.classification,
    ));
    out.push_str("-- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n");
    for op in lowered {
        out.push_str(&format!("-- {label}\n", label = op.label));
        out.push_str(op.up.trim_end_matches('\n'));
        out.push_str("\n\n");
    }
    out
}

/// Render the down-side SQL file. Same shape as up; lossy ops emit
/// SQL-comment placeholders that the operator must hand-edit.
fn compose_down_text(version: &str, delta: &SchemaDelta, lowered: &[OperationSql]) -> String {
    let mut out = String::with_capacity(lowered.iter().map(|o| o.down.len()).sum::<usize>() + 256);
    out.push_str("-- Djogi composed migration — down\n");
    out.push_str(&format!("-- Version: {version}\n"));
    out.push_str(&format!(
        "-- Bucket:  {database}/{app}\n",
        database = delta.bucket.database,
        app = super::target::app_dirname(&delta.bucket.app),
    ));
    out.push_str("-- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n");
    // Reverse order — drop operations roll back in reverse order.
    for op in lowered.iter().rev() {
        out.push_str(&format!("-- {label}\n", label = op.label));
        if let Some(lossy) = &op.lossy {
            out.push_str(&format!(
                "-- LOSSY: {kind:?} — {detail}\n",
                kind = lossy.kind,
                detail = lossy.detail
            ));
        }
        out.push_str(op.down.trim_end_matches('\n'));
        out.push_str("\n\n");
    }
    out
}

// ── Atomic write helpers ───────────────────────────────────────────────────

fn ensure_parent(path: &Path) -> Result<(), ComposeError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| ComposeError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

/// Write `bytes` to a sibling temp file next to `final_path` and
/// fsync. Returns the temp path so the caller can promote it via
/// [`promote_tmp`] once every sibling write succeeds.
fn atomic_write(final_path: &Path, bytes: &[u8]) -> Result<PathBuf, ComposeError> {
    use std::io::Write as _;
    let pid = std::process::id();
    let mut file_name = final_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    file_name.push(format!(".tmp.{pid}"));
    let tmp = final_path.with_file_name(file_name);
    let mut f = fs::File::create(&tmp).map_err(|e| ComposeError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    f.write_all(bytes).map_err(|e| ComposeError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    f.sync_all().map_err(|e| ComposeError::Io {
        path: tmp.clone(),
        source: e,
    })?;
    Ok(tmp)
}

fn promote_tmp(tmp: &Path, final_path: &Path) -> Result<(), ComposeError> {
    fs::rename(tmp, final_path).map_err(|e| ComposeError::Io {
        path: final_path.to_path_buf(),
        source: e,
    })
}

/// Ensure the per-database pending dir exists. Useful as a pre-flight
/// for callers that want to confirm the workspace is writable.
pub fn prepare_pending_dirs(workspace_root: &Path, bucket: &BucketKey) -> Result<(), ComposeError> {
    let dir = pending_database_dir(workspace_root, &bucket.database);
    fs::create_dir_all(&dir).map_err(|e| ComposeError::Io {
        path: dir,
        source: e,
    })
}

/// Format an [`OffsetDateTime`] as RFC 3339 UTC with second
/// precision, mirroring [`super::projection::rfc3339_now_seconds`]
/// but accepting an explicit instant.
fn format_rfc3339_seconds(instant: OffsetDateTime) -> String {
    let utc = instant.to_offset(time::UtcOffset::UTC);
    let secs = utc.unix_timestamp();
    let trimmed = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(utc);
    let format = time::format_description::well_known::Rfc3339;
    trimmed
        .format(&format)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::guard::acquire as acquire_guard;
    use crate::migrate::schema::{
        ColumnSchema, PkKindSchema, PrimaryKeySchema, SNAPSHOT_FORMAT_VERSION, TableSchema,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn at(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        let date = time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day)
            .unwrap();
        let time = time::Time::from_hms(hour, minute, second).unwrap();
        date.with_time(time).assume_utc()
    }

    fn temp_workspace(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("djogi-compose-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn lock_for(workspace: &Path) -> WorkspaceGuard {
        let lock_path = workspace.join(super::super::guard::LOCK_FILE_NAME);
        acquire_guard(&lock_path, Duration::from_secs(5)).expect("lock")
    }

    fn empty_snapshot(bucket: &BucketKey) -> AppliedSchema {
        AppliedSchema {
            djogi_version: env!("CARGO_PKG_VERSION").to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec![bucket.app.clone()],
        }
    }

    fn snapshot_with_widgets(bucket: &BucketKey) -> AppliedSchema {
        let mut s = empty_snapshot(bucket);
        s.models.insert(
            "widgets".to_string(),
            TableSchema {
                app: if bucket.app.is_empty() {
                    None
                } else {
                    Some(bucket.app.clone())
                },
                columns: vec![ColumnSchema {
                    check: None,
                    default_sql: Some("generate_id_desc()".to_string()),
                    foreign_key: None,
                    index_type: None,
                    indexed: false,
                    max_length: None,
                    name: "id".to_string(),
                    nullable: false,
                    on_delete: None,
                    outbox_exclude: false,
                    rationale: None,
                    relation_kind: None,
                    renamed_from: None,
                    sequence_within: None,
                    sql_type: "BIGINT".to_string(),
                    unique: false,
                }],
                fts: None,
                is_through: false,
                moved_from_app: None,
                partition: None,
                primary_key: PrimaryKeySchema {
                    columns: vec!["id".to_string()],
                    kind: PkKindSchema::HeerIdRecencyBiased,
                },
                rationale: None,
                renamed_from: None,
                rls_enabled: false,
                table: "widgets".to_string(),
                tenant_key: None,
            },
        );
        s
    }

    fn global_bucket() -> BucketKey {
        BucketKey {
            database: "main".into(),
            app: "".into(),
        }
    }

    #[test]
    fn empty_models_and_snapshots_returns_nothing_to_compose() {
        let work = temp_workspace("empty");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), empty_snapshot(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "noop",
            allow_destructive: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
        };
        let err = compose(req).expect_err("noop");
        assert!(matches!(err, ComposeError::NothingToCompose));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn add_table_writes_three_files_atomically() {
        let work = temp_workspace("add_table");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
        };
        let report = compose(req).expect("compose");
        assert_eq!(report.composed_buckets.len(), 1);
        let cb = &report.composed_buckets[0];
        assert!(cb.up_sql_path.exists());
        assert!(cb.down_sql_path.exists());
        assert!(cb.pending_json_path.exists());
        // Up SQL must contain CREATE TABLE.
        let up = fs::read_to_string(&cb.up_sql_path).unwrap();
        assert!(up.contains("CREATE TABLE \"widgets\""));
        // Pending JSON must round-trip through PendingPlan.
        let pending_bytes = fs::read(&cb.pending_json_path).unwrap();
        let pending: PendingPlan = serde_json::from_slice(&pending_bytes).expect("parse");
        assert_eq!(pending.bucket_app, "");
        assert_eq!(pending.bucket_database, "main");
        assert!(pending.checksum_up.starts_with("V1:"));
        assert!(pending.version.starts_with("V20260425010203__"));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn destructive_classification_requires_allow_destructive() {
        let work = temp_workspace("destructive");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        // Snapshot has widgets, models do not — drop table.
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), empty_snapshot(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "drop widgets",
            allow_destructive: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
        };
        let err = compose(req).expect_err("destructive");
        assert!(matches!(
            err,
            ComposeError::DestructiveRequiresAllowDestructive { .. }
        ));
        // No file should have been written.
        let dir = bucket_dir(&work, &bucket);
        let count = fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        assert_eq!(count, 0, "no SQL written on destructive refusal");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn destructive_with_allow_destructive_writes_files() {
        let work = temp_workspace("destructive_ok");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), empty_snapshot(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "drop widgets",
            allow_destructive: true,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
        };
        let report = compose(req).expect("compose");
        assert_eq!(report.composed_buckets.len(), 1);
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn tombstoned_app_without_flag_emits_d011() {
        let work = temp_workspace("tombstone_no_flag");
        let guard = lock_for(&work);
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let snapshots = BTreeMap::new();
        let app = AppLifecycle {
            label: "billing".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: true,
        };
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: std::slice::from_ref(&app),
            name: "tomb",
            allow_destructive: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
        };
        let err = compose(req).expect_err("tombstone");
        match err {
            ComposeError::TombstonedAppRequiresAllowDestructive { text, .. } => {
                assert!(text.contains("D011"), "must surface D011 token: {text}");
                assert!(text.contains("billing"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn rename_app_emits_rename_op_when_destination_has_pending_changes() {
        let work = temp_workspace("rename_app");
        let guard = lock_for(&work);
        // Source bucket holds the prior snapshot.
        let old_bucket = BucketKey {
            database: "main".into(),
            app: "oldname".into(),
        };
        let new_bucket = BucketKey {
            database: "main".into(),
            app: "newname".into(),
        };
        // Fresh state: snapshot has the old bucket with widgets;
        // the model state has the new bucket with widgets too.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(old_bucket.clone(), snapshot_with_widgets(&old_bucket));
        let mut models = BTreeMap::new();
        // Add a NEW widget shape so the diff is non-empty besides the
        // rename. We do that by adding a new column to widgets.
        let mut new_schema = snapshot_with_widgets(&new_bucket);
        let new_table = new_schema.models.get_mut("widgets").unwrap();
        new_table.columns.push(ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: "color".to_string(),
            nullable: true,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "TEXT".to_string(),
            unique: false,
        });
        models.insert(new_bucket.clone(), new_schema);
        let app = AppLifecycle {
            label: "newname".to_string(),
            database: "main".to_string(),
            renamed_from: Some("oldname".to_string()),
            tombstone: false,
        };
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: std::slice::from_ref(&app),
            name: "rename newname",
            allow_destructive: true,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
        };
        let report = compose(req).expect("compose");
        // The destination bucket (newname) should have the RenameApp
        // op visible in the up SQL.
        let dest = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == new_bucket)
            .expect("destination composed");
        let up = fs::read_to_string(&dest.up_sql_path).unwrap();
        assert!(
            up.contains("RenameApp"),
            "up SQL must label the RenameApp op: {up}"
        );
        assert!(up.contains("oldname"));
        assert!(up.contains("newname"));
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn overwrite_on_same_name_replaces_artifacts_byte_stably() {
        let work = temp_workspace("overwrite");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));
        let now = at(2026, 4, 25, 1, 2, 3);
        let req1 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            now,
            _guard: &guard,
        };
        let r1 = compose(req1).expect("first");
        let up1 = fs::read(&r1.composed_buckets[0].up_sql_path).unwrap();
        let pending1 = fs::read(&r1.composed_buckets[0].pending_json_path).unwrap();
        // Second run with the same inputs and the same `now`.
        let req2 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            now,
            _guard: &guard,
        };
        let r2 = compose(req2).expect("second");
        let up2 = fs::read(&r2.composed_buckets[0].up_sql_path).unwrap();
        let pending2 = fs::read(&r2.composed_buckets[0].pending_json_path).unwrap();
        assert_eq!(up1, up2, "up SQL must be byte-identical");
        assert_eq!(pending1, pending2, "pending JSON must be byte-identical");
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn pending_json_round_trips_through_serde() {
        let bucket = global_bucket();
        let plan = PendingPlan {
            format_version: PENDING_FORMAT_VERSION.to_string(),
            bucket_database: bucket.database.clone(),
            bucket_app: bucket.app.clone(),
            version: "V20260425010203__add_widgets".to_string(),
            slug: "add_widgets".to_string(),
            model_snapshot: empty_snapshot(&bucket),
            checksum_up: "V1:".to_string() + &"a".repeat(64),
            checksum_down: None,
            composed_at: "2026-04-25T01:02:03Z".to_string(),
        };
        let bytes = serde_json::to_vec(&plan).unwrap();
        let parsed: PendingPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, plan);
    }
}
