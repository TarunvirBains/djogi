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

/// One restore point captured before a tmp file was promoted onto a
/// destination that already had bytes on it.
///
/// Per Codex B-10: `promote_tmp` overwrites the final path via
/// `fs::rename`. Without a backup of the prior bytes, a later failure
/// in the same compose sequence cannot restore the original file —
/// the rollback only knew to `remove_file(final_path)`, leaving the
/// workspace in a half-state. This struct carries both the final path
/// (where the new bytes live after a successful promote) and the
/// backup path (where the prior bytes were copied just before the
/// rename). On commit we delete the backup; on failure we restore the
/// backup over the final path.
struct RestorePoint {
    /// The artifact's final path on disk (the post-promote location).
    final_path: PathBuf,
    /// Sibling backup file that holds the pre-overwrite bytes.
    /// `None` when no prior file existed and the promote was a fresh
    /// create rather than an overwrite — nothing to restore.
    backup_path: Option<PathBuf>,
}

/// RAII rollback guard for atomic compose writes.
///
/// Tracks three parallel cleanup queues:
///
/// 1. `tmps` — staged `<final>.tmp.<pid>` files that have been
///    written but not yet promoted. These are removed on failure.
/// 2. `restore_points` — files that have already been renamed into
///    their final location, possibly OVER an existing file. On failure
///    we restore the prior bytes (via the backup path) when one was
///    captured, otherwise we delete the freshly-promoted file. This
///    addresses Codex B-10: the previous shape only deleted the final
///    path on rollback, which silently lost the original content for
///    overwrite cases.
/// 3. `entry_renames` — entries that were moved from one directory to
///    another by [`rename_old_bucket_folder`]. On failure we move them
///    back. This addresses Codex B-11: the merge loop touched many
///    files and a mid-loop failure left partial state untracked.
///
/// On a successful sequence the caller invokes [`commit`](Self::commit)
/// to drain every queue (and delete the backups) — the [`Drop`] impl
/// then runs as a no-op. On any failure path the guard goes out of
/// scope without `commit` being called and every tracked artifact is
/// rolled back via best-effort filesystem ops.
struct WriteRollback {
    tmps: Vec<PathBuf>,
    restore_points: Vec<RestorePoint>,
    entry_renames: Vec<(PathBuf, PathBuf)>,
}

impl WriteRollback {
    fn new() -> Self {
        Self {
            tmps: Vec::new(),
            restore_points: Vec::new(),
            entry_renames: Vec::new(),
        }
    }

    /// Track a staged tmp file — removed on failure if not yet promoted.
    fn track_tmp(&mut self, path: PathBuf) {
        self.tmps.push(path);
    }

    /// Mark a tmp as successfully promoted to its final path. The tmp
    /// is removed from the tmp queue (the file no longer exists at
    /// that path) and a [`RestorePoint`] is recorded so a later failure
    /// rolls the final path back. `backup_path` is `None` when the
    /// promote was a fresh create (no existing bytes were overwritten),
    /// in which case the rollback simply deletes the final path; when
    /// `Some`, the rollback restores the backup bytes back over
    /// `final_path` per Codex B-10's overwrite-safe contract.
    fn promote(&mut self, tmp: &Path, final_path: PathBuf, backup_path: Option<PathBuf>) {
        if let Some(idx) = self.tmps.iter().position(|p| p == tmp) {
            self.tmps.remove(idx);
        }
        self.restore_points.push(RestorePoint {
            final_path,
            backup_path,
        });
    }

    /// Track an entry rename performed during the post-compose folder
    /// merge — the pair is `(from, to)`. On failure we move it back
    /// from `to` to `from`. Per Codex B-11 the merge loop must be
    /// undoable so a mid-loop failure does not leak partial state.
    fn track_entry_rename(&mut self, from: PathBuf, to: PathBuf) {
        self.entry_renames.push((from, to));
    }

    /// Drain every queue without running cleanup — call on the
    /// success path to consume the guard. The committed restore-point
    /// backups are deleted here so a successful compose leaves no
    /// `.bak.<pid>` siblings on disk.
    fn commit(mut self) {
        self.tmps.clear();
        // Backups exist only because the promote overwrote a prior
        // file. On commit (success path) we delete each backup.
        for rp in self.restore_points.drain(..) {
            if let Some(backup) = rp.backup_path {
                let _ = fs::remove_file(&backup);
            }
        }
        self.entry_renames.clear();
    }
}

impl Drop for WriteRollback {
    fn drop(&mut self) {
        // Best-effort cleanup. Errors are intentionally swallowed —
        // we cannot panic from Drop, and the operator already saw the
        // primary error that triggered the rollback. A dangling tmp
        // file would only matter if the operator immediately re-ran
        // compose; the next tmp uses a fresh `<pid>` suffix so even
        // a missed cleanup never collides.
        for p in self.tmps.drain(..) {
            let _ = fs::remove_file(&p);
        }
        // Restore in reverse order — the LIFO unwind keeps the
        // filesystem state consistent (later promotes are undone
        // before earlier ones).
        for rp in self.restore_points.drain(..).rev() {
            match rp.backup_path {
                Some(backup) => {
                    // Best-effort restore: rename backup back over the
                    // final path. If the rename fails (e.g. the backup
                    // disappeared) we fall back to deleting the new
                    // bytes so the workspace at least matches the
                    // "fresh-create" rollback path.
                    if fs::rename(&backup, &rp.final_path).is_err() {
                        let _ = fs::remove_file(&rp.final_path);
                        let _ = fs::remove_file(&backup);
                    }
                }
                None => {
                    let _ = fs::remove_file(&rp.final_path);
                }
            }
        }
        // Per Codex B-11: undo every tracked entry rename. We move
        // each `to` back to its prior `from` location.
        //
        // Codex round-3 B-11 testing-gap note: this rollback path is
        // reachable in principle (a `fs::rename` call inside the merge
        // loop could fail mid-iteration on out-of-disk, EPERM, or a
        // TOCTOU race against the pre-flight check), but in practice
        // the pre-flight collision scan in `rename_old_bucket_folder`
        // catches every deterministically-reachable failure before any
        // entry has been moved — so this branch executes zero queue
        // entries on every test run. A non-vacuous test would have to
        // simulate a mid-loop kernel-level failure (permission flip
        // between iterations, disk-full on the second move, etc.) and
        // those are not portably reproducible from a unit test
        // harness. The rollback queue is kept alive defensively so a
        // future change to the pre-flight (or a TOCTOU race in
        // production) cannot leave the workspace half-merged. See
        // `b11_pre_flight_pre_empts_mid_loop_rollback` for the
        // documented gap.
        for (from, to) in self.entry_renames.drain(..).rev() {
            let _ = fs::rename(&to, &from);
        }
    }
}

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
    /// D013 — the destination SQL file already exists and its bytes
    /// do NOT match what the deterministic emitter would freshly
    /// produce. That means the operator hand-edited the migration
    /// after compose ran it the first time. Compose refuses to
    /// overwrite without an explicit `--force-overwrite` opt-in.
    ///
    /// The check protects BOTH up and down SQL — the `side` field
    /// disambiguates which file diverged so the diagnostic text
    /// names the offending file.
    HandEditedMigrationWouldBeOverwritten {
        /// Affected bucket.
        bucket: BucketKey,
        /// Path to the file whose bytes diverge from the freshly-
        /// emitted SQL. When both up and down were edited the up
        /// path is reported (the up file is what runs first; the
        /// operator typically inspects it first).
        path: PathBuf,
        /// Pre-formatted diagnostic message — `D013: hand-edited
        /// migration would be overwritten; pass --force-overwrite to
        /// discard your edits` plus which side (up / down / both) was
        /// edited.
        text: String,
    },
    /// Codex B-11 — `rename_old_bucket_folder` would have to merge the
    /// OLD app's directory into a NEW directory that already contains
    /// conflicting entries. The old shape attempted a non-atomic merge
    /// loop; per round-2 we now refuse fail-fast so the operator
    /// resolves the conflict explicitly instead of silently leaving a
    /// partial-merge state on disk.
    FolderRenameTargetCollision {
        /// Source directory (the OLD app's bucket dir).
        from: PathBuf,
        /// Destination directory whose entries collided.
        to: PathBuf,
        /// One offending entry name — included so the operator can
        /// move or delete it before re-running compose.
        offending_entry: String,
    },
    /// B-4r (Codex round-3) — the differ surfaced a structured
    /// `DiffError` (e.g. a PK-flip transitive FK closure exceeded
    /// the depth contract). Compose rendered the error verbatim
    /// rather than letting the panic unwind the run.
    Diff(super::diff::DiffError),
    /// Phase 0 auto-emit failed. Track 0 (sub-step 0.3) wired
    /// `migrations compose` to emit a Phase 0 bootstrap migration
    /// before its delta-based work for any database that doesn't
    /// already have one. The wrapped error names the failing step
    /// (composition vs. filesystem write vs. pending-JSON serialize).
    PhaseZeroAutoEmit(super::bootstrap::AutoEmitError),
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
            Self::HandEditedMigrationWouldBeOverwritten { text, .. } => f.write_str(text),
            Self::FolderRenameTargetCollision {
                from,
                to,
                offending_entry,
            } => write!(
                f,
                "folder rename would collide at {to_path}: entry \"{offending_entry}\" already exists \
                 (source: {from_path}); resolve manually before re-running compose",
                from_path = from.display(),
                to_path = to.display(),
            ),
            Self::Diff(e) => write!(f, "differ refused: {e}"),
            Self::PhaseZeroAutoEmit(e) => write!(f, "{e}"),
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
    /// Operator opt-in for overwriting hand-edited migration files.
    /// When `false` (the default), compose refuses with D013 when
    /// EITHER the existing up SQL or the existing down SQL bytes
    /// diverge from what the deterministic emitter would freshly
    /// produce — that means the operator hand-edited the file after
    /// the prior compose. When `true`, compose discards the edits and
    /// rewrites the files with freshly-emitted SQL.
    ///
    /// **Implementation detail.** The divergence check is a
    /// byte-equality compare between the existing file's content and
    /// the freshly-emitted bytes — NOT a checksum read from the
    /// pending JSON. Because the SQL emitter is deterministic (same
    /// inputs always produce the same output bytes), byte-equality
    /// is exactly equivalent to a checksum match without re-deriving
    /// the checksum or parsing the pending JSON. The check covers
    /// both the up side and the down side.
    pub force_overwrite: bool,
    /// Compose-time clock, used as the version-prefix instant.
    /// Production callers pass `OffsetDateTime::now_utc()`; tests
    /// pin a deterministic value so the version ID is byte-stable.
    pub now: OffsetDateTime,
    /// Witness-typed file lock — compose mutates `<workspace>/migrations/`
    /// and `<workspace>/target/djogi_pending/`, both of which require
    /// the workspace lock per the v3 §6 file-lock contract.
    pub _guard: &'a WorkspaceGuard,
    /// Join-table cutover layout for any T9 PK-flip group emitted by
    /// the differ. `None` defaults to
    /// [`super::diff::PkFlipJoinTableOption::OptionA`] — single
    /// mega-transaction across both parents and the join table per
    /// playbook §7. `Some(OptionB)` selects sequential per-parent
    /// flips. Production callers pass the operator's
    /// [`crate::config::MigrateConfig::pk_flip_join_table_option`]
    /// converted via
    /// [`super::diff::PkFlipJoinTableOption::from_config_char`].
    pub pk_flip_join_table_option: Option<super::diff::PkFlipJoinTableOption>,
    /// Track 0 — opt out of Phase 0 bootstrap auto-emit.
    ///
    /// Production callers leave this `false` (the default behaviour):
    /// every database referenced in `models` ∪ `apps` that doesn't
    /// already have a Phase 0 migration on disk receives one before
    /// the regular delta-based work runs.
    ///
    /// Tests that exercise compose's lower-level write / rollback
    /// machinery in isolation (no real schema, just the file dance)
    /// set this to `true` to keep the per-bucket directory free of
    /// the auto-emitted Phase 0 artefacts. The skip is a test-only
    /// affordance — the CLI / production paths always go through the
    /// full auto-emit flow.
    ///
    /// Not adopter API. Setting this `true` from outside the crate
    /// bypasses Phase 0 and is unsupported.
    #[doc(hidden)]
    pub skip_phase_zero_auto_emit: bool,
}

/// Successful-compose report. Returned per-bucket so the caller can
/// log structured progress.
#[derive(Debug, Clone)]
pub struct ComposeReport {
    /// One entry per bucket that received a compose. Empty when
    /// every bucket was already in sync (callers handle this via the
    /// [`ComposeError::NothingToCompose`] error path).
    pub composed_buckets: Vec<ComposedBucket>,
    /// One entry per database that received a Phase 0 bootstrap
    /// migration during this compose run. Track 0 (sub-step 0.3)
    /// wired auto-emit so any database whose
    /// `migrations/<db>/_global_/V00000000000000__phase_zero_bootstrap.sql`
    /// is missing receives one before the delta-based work runs.
    /// Empty when every database already had Phase 0 on disk.
    pub emitted_phase_zero: Vec<super::bootstrap::EmittedPhaseZero>,
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

/// Errors surfaced by [`parse_pending_bytes`].
///
/// A separate type from [`ComposeError`] because the pending-load
/// path is run in build.rs / status / verify contexts that don't
/// touch the workspace lock or the SQL emitter — flowing those
/// errors into [`ComposeError`] would leak unrelated variants.
#[derive(Debug)]
pub enum PendingLoadError {
    /// JSON parse error.
    Parse {
        path: Option<PathBuf>,
        source: serde_json::Error,
    },
    /// `format_version` mismatch — declared version doesn't match
    /// [`PENDING_FORMAT_VERSION`]. Caught by the peek BEFORE
    /// structural deserialize so the operator gets an actionable
    /// upgrade message instead of a `deny_unknown_fields` shower.
    UnsupportedFormatVersion {
        found: String,
        expected: &'static str,
        path: Option<PathBuf>,
    },
}

impl std::fmt::Display for PendingLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingLoadError::Parse { path, source } => match path {
                Some(p) => write!(f, "pending parse at {}: {source}", p.display()),
                None => write!(f, "pending parse: {source}"),
            },
            PendingLoadError::UnsupportedFormatVersion {
                found,
                expected,
                path,
            } => match path {
                Some(p) => write!(
                    f,
                    "pending JSON format version '{found}' at {} is not supported by this Djogi (expected '{expected}'); upgrade or check out a newer djogi",
                    p.display()
                ),
                None => write!(
                    f,
                    "pending JSON format version '{found}' is not supported by this Djogi (expected '{expected}'); upgrade or check out a newer djogi"
                ),
            },
        }
    }
}

impl std::error::Error for PendingLoadError {}

/// Codex B-7 — parse a pending JSON byte slice with a format-version
/// peek before structural deserialize.
///
/// Mirrors the snapshot loader's two-stage pattern: a permissive
/// `serde_json::Value` parse first to inspect the top-level
/// `format_version`, then a strict
/// [`serde(deny_unknown_fields)`]-driven structural parse. Future
/// pending-format versions surface
/// [`PendingLoadError::UnsupportedFormatVersion`] with both the found
/// and expected versions so the operator's message is actionable.
///
/// `path` is purely for error reporting; pass `None` when the bytes
/// come from memory.
pub fn parse_pending_bytes(
    bytes: &[u8],
    path: Option<PathBuf>,
) -> Result<PendingPlan, PendingLoadError> {
    // Stage 1 — peek at `format_version`. A future version with
    // additional fields would otherwise trip `deny_unknown_fields`
    // in stage 2 with a cryptic error.
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Some(serde_json::Value::String(found)) = map.get("format_version")
        && found != PENDING_FORMAT_VERSION
    {
        return Err(PendingLoadError::UnsupportedFormatVersion {
            found: found.clone(),
            expected: PENDING_FORMAT_VERSION,
            path,
        });
    }
    // Stage 2 — strict structural parse.
    let plan: PendingPlan = serde_json::from_slice(bytes).map_err(|e| PendingLoadError::Parse {
        path: path.clone(),
        source: e,
    })?;
    if plan.format_version != PENDING_FORMAT_VERSION {
        return Err(PendingLoadError::UnsupportedFormatVersion {
            found: plan.format_version,
            expected: PENDING_FORMAT_VERSION,
            path,
        });
    }
    Ok(plan)
}

/// Convenience wrapper around [`parse_pending_bytes`] for on-disk
/// pending JSON files.
pub fn load_pending(path: &Path) -> Result<PendingPlan, PendingLoadError> {
    let bytes = fs::read(path).map_err(|e| PendingLoadError::Parse {
        path: Some(path.to_path_buf()),
        source: serde_json::Error::io(e),
    })?;
    parse_pending_bytes(&bytes, Some(path.to_path_buf()))
}

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
    // 0. Phase 0 auto-emit (Track 0, sub-step 0.3) — for any database
    //    referenced in the inputs that doesn't already have a Phase 0
    //    bootstrap migration on disk, emit one. This runs BEFORE the
    //    tombstone / differ / classification / write logic because
    //    Phase 0 is independent of the descriptor delta — it's
    //    framework bootstrap (HeeRanjID schema + Postgres extensions
    //    + node-id GUC) that every subsequent migration depends on.
    //
    //    Idempotent — emits nothing when the marker file already
    //    exists. Once emitted, Phase 0 is a regular committed
    //    migration that the runner / `db reset` replays in lexical
    //    version order (the all-zero `V00000000000000` prefix sorts
    //    before any operator-composed migration).
    //
    //    Crucially, Phase 0 emission is NOT gated on "delta has
    //    operations" — a workspace can validly compose Phase 0 even
    //    when no model changes need a regular migration. The downstream
    //    `NothingToCompose` check below considers ONLY the regular
    //    delta path; Phase 0 emissions count as compose progress on
    //    their own (the report carries them in `emitted_phase_zero`).
    let emitted_phase_zero = if req.skip_phase_zero_auto_emit {
        Vec::new()
    } else {
        super::bootstrap::ensure_phase_zero_emitted(
            req.workspace_root,
            req.models,
            req.apps,
            req.now,
            req._guard,
        )
        .map_err(ComposeError::PhaseZeroAutoEmit)?
    };

    // 1. Collect tombstone violations BEFORE any work — fail loudly
    //    when an active model OR a stale snapshot still references a
    //    tombstoned app.
    //
    //    Per Codex B-4: D011 fires whenever a tombstoned app still has
    //    schema state to drop, regardless of whether that state lives
    //    in `models` (developer hasn't yet removed the structs) or in
    //    the snapshot (developer removed the structs but the schema
    //    is still applied to the database). The previous guard
    //    `!s.models.is_empty()` skipped the snapshot-only path and
    //    let the destructive classification fire generically — losing
    //    the D011 specificity.
    if !req.allow_destructive {
        for app in req.apps {
            if !app.tombstone {
                continue;
            }
            let bucket_for_app = BucketKey {
                database: app.database.clone(),
                app: app.label.clone(),
            };
            // The tombstoned app has schema state to drop iff EITHER
            // `models` carries at least one model for the bucket OR
            // the snapshot does. Either way the operator needs the
            // `--allow-destructive` opt-in.
            let models_has_state = req
                .models
                .get(&bucket_for_app)
                .is_some_and(|s| !s.models.is_empty());
            let snapshot_has_state = req
                .snapshots
                .get(&bucket_for_app)
                .is_some_and(|s| !s.models.is_empty());
            if models_has_state || snapshot_has_state {
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

    // 2. Per Codex B-9: rewrite snapshot bucket keys for renamed apps
    //    BEFORE running the differ. The on-disk SQL tables don't move
    //    when an app renames — only the `app_label` ledger column and
    //    the `migrations/<db>/<app>/` folder do. The pre-rename
    //    snapshot still describes the same physical tables; under the
    //    NEW app label they are unchanged. By rewriting the OLD
    //    bucket's snapshot key to NEW before diffing, the differ sees
    //    the tables as already-present on both sides and emits no
    //    spurious DropTable on OLD / AddTable on NEW. Without this
    //    rewrite a rename would always require `--allow-destructive`
    //    even though the operation is metadata-only.
    let snapshots_for_diff = remap_snapshots_for_renames(req.snapshots, req.apps);

    // 3. Run the differ across the (possibly remapped) bucket map.
    //    B-4r (Codex round-3): the differ now returns Result;
    //    cascade-depth blow-outs surface as `ComposeError::Diff`
    //    rather than panicking.
    let mut deltas =
        diff_bucket_maps(&snapshots_for_diff, req.models).map_err(ComposeError::Diff)?;

    // 3b. Apply operator-configured join-table cutover layout to every
    //     PK-flip group the differ emitted. Without this step the
    //     `MigrateConfig::pk_flip_join_table_option` knob would have
    //     no effect — the differ defaults every group to Option A and
    //     only this hook overrides it.
    if let Some(option) = req.pk_flip_join_table_option {
        super::diff::apply_pk_flip_join_table_option(&mut deltas, option);
    }

    // 4. Layer in `RenameApp` ops driven by `AppRegistry`'s
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

    // 5. Filter to non-empty deltas. NoOp deltas have classification
    //    `NoOp` and an empty operations vec; skip them. Renamed-only
    //    deltas DO carry operations and survive the filter.
    let mut effective: Vec<SchemaDelta> = deltas
        .into_iter()
        .filter(|d| !d.operations.is_empty() || !matches!(d.classification, Classification::NoOp))
        .collect();

    if effective.is_empty() {
        // Track 0 (sub-step 0.3): when the regular delta path has
        // nothing to do BUT Phase 0 was emitted this run, the compose
        // is NOT a no-op — Phase 0 is real progress that the operator
        // will apply via `migrations apply`. Surface a successful
        // report so the CLI's friendly "composed N phase-zero
        // bootstrap migrations" line prints, instead of the
        // `NothingToCompose` exit-zero quiet path.
        //
        // The reverse case — Phase 0 already on disk AND no delta
        // changes — surfaces `NothingToCompose` as before. That keeps
        // the "all in sync" message intact.
        if !emitted_phase_zero.is_empty() {
            return Ok(ComposeReport {
                composed_buckets: Vec::new(),
                emitted_phase_zero,
            });
        }
        return Err(ComposeError::NothingToCompose);
    }

    // 6. Re-classify deltas that gained injected RenameApp ops and
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

    // 7. Lower each delta to SQL pairs + plan, write all artifacts.
    //
    // The write dance per bucket:
    //   - Compute the lowered SQL pair + checksums.
    //   - Inject the ledger UPDATE leg for any RenameApp ops (Codex
    //     B-5: v3 §6 mandates that the rename-exception ledger UPDATE
    //     ride along with the migration's up/down).
    //   - D013 check: refuse to overwrite a hand-edited file unless
    //     `force_overwrite` is set (Codex B-3 / OQ-08).
    //   - Stage three sibling tmp files (up SQL, down SQL, pending
    //     JSON), tracked under a `WriteRollback` Drop guard so any
    //     mid-sequence failure removes ALL staged tmps + already-
    //     promoted finals (Codex B-2).
    //   - Promote each tmp to its final path; on success commit the
    //     guard.
    //   - Per RenameApp delta, atomically rename the OLD bucket
    //     directory to the NEW bucket directory after artifacts land
    //     (Codex B-5).
    let slug = sanitize_slug(req.name);
    let prefix = version_prefix(req.now);
    let version = version_id(&prefix, &slug);
    let composed_at = format_rfc3339_seconds(req.now);

    // Outer rollback guard tracks every artifact across all buckets
    // so a failure on bucket N cleans up every artifact already
    // written by buckets 0..N too.
    let mut rollback = WriteRollback::new();
    let mut composed_buckets: Vec<ComposedBucket> = Vec::with_capacity(effective.len());
    let mut pending_folder_renames: Vec<(PathBuf, PathBuf)> = Vec::new();

    let result: Result<(), ComposeError> = (|| {
        for delta in &effective {
            // Shouldn't fail — we validated classifications above.
            let mut lowered = lower_delta(delta).map_err(ComposeError::SqlEmit)?;
            let _plan: MigrationPlan = plan_delta(delta).map_err(ComposeError::SqlEmit)?;

            // Codex B-5: For each RenameApp op, append an OperationSql
            // that updates `djogi_schema_migrations.app_label` so the
            // ledger is consistent with the new bucket name. The
            // metadata-only OperationSql produced by the standard
            // emitter carries only comments; we layer the real DDL
            // here so it's hashed into `checksum_up` and reviewable
            // in the on-disk SQL file.
            let mut folder_renames_for_delta: Vec<(String, String)> = Vec::new();
            for op in &delta.operations {
                if let SchemaOperation::RenameApp { from, to } = op {
                    lowered.push(emit_rename_app_ledger_update(
                        &delta.bucket.database,
                        from,
                        to,
                    ));
                    folder_renames_for_delta.push((from.clone(), to.clone()));
                }
            }

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

            // Codex B-3 — D013 hand-edit protection.
            //
            // Per round-2: protect BOTH up AND down SQL. If either
            // file already exists and its current bytes differ from
            // what compose would emit fresh, the operator has hand
            // edited the migration. Without `force_overwrite` we
            // refuse to clobber. The comparison uses full byte
            // equality (not a separate checksum) because the emitter
            // is deterministic — same inputs always produce the same
            // bytes — so byte-equality is exactly equivalent to a
            // checksum match without re-derivation.
            if !req.force_overwrite {
                check_no_hand_edit(
                    &up_path,
                    up_sql.as_bytes(),
                    &down_path,
                    down_sql.as_bytes(),
                    &delta.bucket,
                )?;
            }

            // Stage tmp siblings.
            ensure_parent(&up_path)?;
            ensure_parent(&pending_path)?;
            let up_tmp = atomic_write(&up_path, up_sql.as_bytes())?;
            rollback.track_tmp(up_tmp.clone());

            let down_tmp = atomic_write(&down_path, down_sql.as_bytes())?;
            rollback.track_tmp(down_tmp.clone());

            let pending_tmp = atomic_write(&pending_path, &pending_bytes)?;
            rollback.track_tmp(pending_tmp.clone());

            // Promote tmps. Order: up SQL, down SQL, pending JSON.
            // Per Codex B-10 each promote captures any prior bytes
            // into a sibling backup file BEFORE renaming the tmp into
            // place; the `WriteRollback` guard records the backup
            // alongside the final path so a later failure restores
            // the original content. On commit (success path) the
            // backups are deleted.
            let up_backup = promote_tmp_with_backup(&up_tmp, &up_path)?;
            rollback.promote(&up_tmp, up_path.clone(), up_backup);

            let down_backup = promote_tmp_with_backup(&down_tmp, &down_path)?;
            rollback.promote(&down_tmp, down_path.clone(), down_backup);

            let pending_backup = promote_tmp_with_backup(&pending_tmp, &pending_path)?;
            rollback.promote(&pending_tmp, pending_path.clone(), pending_backup);

            // Codex B-5: queue any RenameApp folder moves. We perform
            // them after every artifact write succeeds because a folder
            // rename is hard to roll back atomically in conjunction with
            // the file writes — the conservative posture is to write
            // first, rename second.
            for (from_label, _to_label) in folder_renames_for_delta {
                let from_bucket = BucketKey {
                    database: delta.bucket.database.clone(),
                    app: from_label,
                };
                let from_dir = bucket_dir(req.workspace_root, &from_bucket);
                let to_dir = bucket_dir(req.workspace_root, &delta.bucket);
                pending_folder_renames.push((from_dir, to_dir));
            }

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

    // `rollback` Drop will clean up every tracked tmp + restore every
    // overwrite backup on the early-return; nothing else to do here.
    result?;

    // All file writes succeeded. Apply the queued folder renames for
    // RenameApp ops (Codex B-5). Per round-2 B-11 the merge step
    // tracks every entry move on the same `rollback` guard so a
    // mid-loop failure rolls back every already-moved entry too.
    for (from_dir, to_dir) in &pending_folder_renames {
        rename_old_bucket_folder(from_dir, to_dir, &mut rollback)?;
    }

    // All work succeeded — release the rollback guard. This deletes
    // every backup file captured during promote and clears every
    // entry-rename tracking entry.
    rollback.commit();
    Ok(ComposeReport {
        composed_buckets,
        emitted_phase_zero,
    })
}

/// Codex B-3 / D013 — refuse to overwrite a hand-edited migration.
///
/// Compares the existing up AND down SQL files' bytes to what compose
/// would emit fresh. When EITHER side's existing bytes differ from
/// the freshly-emitted bytes the operator has hand edited the
/// migration; we surface
/// [`ComposeError::HandEditedMigrationWouldBeOverwritten`] (D013)
/// rather than silently clobber. Per round-2 the down side was
/// previously unprotected — a hand-edit there would have been
/// silently overwritten.
///
/// We compare full bytes rather than a separate checksum because
/// `compose_up_text` / `compose_down_text` are deterministic — same
/// inputs always produce the same bytes — so byte-equality is
/// exactly equivalent to "checksum matches" without needing a
/// reverse-engineering pass over the formatted SQL file. (Per Codex
/// round-2 A-2: this is the canonical D013 check; the doc comment on
/// `ComposeError::HandEditedMigrationWouldBeOverwritten` describes
/// the byte-equality semantics directly.)
///
/// The reported `path` and `side` describe which side was edited:
///
/// - Up only edited → `path = up_path`, side label "up".
/// - Down only edited → `path = down_path`, side label "down".
/// - Both edited → `path = up_path`, side label "up and down" (the up
///   path is reported because the operator typically inspects the up
///   file first).
///
/// Returns `Ok(())` when:
///
/// - Both files do not exist (first compose for this bucket).
/// - The existing files' bytes both match the freshly-emitted bytes.
fn check_no_hand_edit(
    up_path: &Path,
    fresh_up_bytes: &[u8],
    down_path: &Path,
    fresh_down_bytes: &[u8],
    bucket: &BucketKey,
) -> Result<(), ComposeError> {
    let up_edited = match fs::read(up_path) {
        Ok(existing) => existing != fresh_up_bytes,
        Err(_) => false, // file missing — fresh compose, no clobber risk.
    };
    let down_edited = match fs::read(down_path) {
        Ok(existing) => existing != fresh_down_bytes,
        Err(_) => false,
    };
    if !up_edited && !down_edited {
        return Ok(());
    }
    // Pick the path + side label to report. When both sides were
    // edited we surface the up path (the operator inspects up first).
    let (reported_path, side_label) = match (up_edited, down_edited) {
        (true, true) => (up_path.to_path_buf(), "up and down"),
        (true, false) => (up_path.to_path_buf(), "up"),
        (false, true) => (down_path.to_path_buf(), "down"),
        (false, false) => unreachable!("guarded above"),
    };
    let text = format!(
        "D013: hand-edited migration would be overwritten ({side_label} side); \
         pass --force-overwrite to discard your edits ({path})",
        path = reported_path.display()
    );
    Err(ComposeError::HandEditedMigrationWouldBeOverwritten {
        bucket: bucket.clone(),
        path: reported_path,
        text,
    })
}

/// Codex B-5 — emit the ledger UPDATE leg for a RenameApp delta.
///
/// Per v3 §6 ("rename exception to append-only ledger"), the ledger
/// row's `app_label` for every prior migration must be updated when an
/// app is renamed. We append this as a real `OperationSql` to the
/// lowered list so it gets:
///
/// 1. Hashed into the up checksum (so verify catches drift).
/// 2. Written into the on-disk SQL file (so the operator can review it).
/// 3. Reversed by the down side (so rollback restores the old label).
fn emit_rename_app_ledger_update(database: &str, from: &str, to: &str) -> OperationSql {
    let _ = database; // database is implicit via the connection target.
    // SQL identifier escape: `from` / `to` are app labels — already
    // strict identifiers per the projection layer. Wrap in single
    // quotes for SQL string literals; we double any embedded `'` for
    // belt-and-braces (a real label can't contain one but the runner
    // does not re-validate).
    let from_escaped = sql_escape_string(from);
    let to_escaped = sql_escape_string(to);
    let up = format!(
        "UPDATE djogi_schema_migrations \
         SET app_label = '{to_escaped}' \
         WHERE app_label = '{from_escaped}';"
    );
    let down = format!(
        "UPDATE djogi_schema_migrations \
         SET app_label = '{from_escaped}' \
         WHERE app_label = '{to_escaped}';"
    );
    OperationSql {
        label: format!("RenameAppLedger {from} -> {to}"),
        up,
        down,
        lossy: None,
    }
}

/// Escape a string for inclusion inside single-quoted SQL literals.
/// Doubles any embedded `'` per the SQL standard. App labels never
/// contain `'` per the projection layer's identifier grammar; we apply
/// the rule defensively so the emitted SQL is robust if that grammar
/// ever loosens.
fn sql_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Codex B-5 / B-11 — atomically rename the OLD bucket directory to
/// the NEW bucket directory.
///
/// Called after every artifact write succeeds so the workspace is
/// consistent on disk. Skips silently when:
///
/// - The OLD directory does not exist (nothing to rename).
/// - The OLD and NEW directories are identical (a same-app
///   "self-rename" is a no-op — should not happen but defensive).
///
/// When the NEW directory already exists (the typical case — compose
/// just wrote artifacts there), we MOVE every entry from OLD to NEW.
/// Per Codex round-2 B-11 each entry move is tracked through the
/// supplied [`WriteRollback`] guard so a mid-loop failure rolls back
/// every already-moved entry.
///
/// Per Codex round-2 B-11 we ALSO refuse fail-fast on a content
/// collision: if any entry under OLD already exists under NEW with
/// a different name-equivalent location, we return
/// [`ComposeError::FolderRenameTargetCollision`] before moving any
/// entry — the prior shape silently skipped collisions and dropped
/// the OLD entry, which conflated two distinct files of the same
/// name. The operator must resolve the collision manually before
/// re-running compose.
fn rename_old_bucket_folder(
    from_dir: &Path,
    to_dir: &Path,
    rollback: &mut WriteRollback,
) -> Result<(), ComposeError> {
    if from_dir == to_dir {
        return Ok(());
    }
    if !from_dir.exists() {
        return Ok(());
    }
    if !to_dir.exists() {
        // Simple rename — no merge needed. We still register the
        // single move with the rollback guard so a later failure
        // (none today, but the hook keeps the contract symmetric)
        // would unwind it.
        ensure_parent(to_dir)?;
        fs::rename(from_dir, to_dir).map_err(|e| ComposeError::Io {
            path: to_dir.to_path_buf(),
            source: e,
        })?;
        rollback.track_entry_rename(from_dir.to_path_buf(), to_dir.to_path_buf());
        return Ok(());
    }
    // NEW dir already exists (compose just wrote artifacts there).
    // Walk OLD, plan each move, fail-fast on any collision, then
    // execute the moves while tracking each on the rollback guard.
    let entries: Vec<PathBuf> = fs::read_dir(from_dir)
        .map_err(|e| ComposeError::Io {
            path: from_dir.to_path_buf(),
            source: e,
        })?
        .filter_map(|res| res.ok().map(|e| e.path()))
        .collect();

    // Codex B-11 — pre-flight collision check. We refuse to
    // silently overwrite any newly-composed artifact in NEW.
    for src in &entries {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = to_dir.join(name);
        if dst.exists() {
            return Err(ComposeError::FolderRenameTargetCollision {
                from: from_dir.to_path_buf(),
                to: to_dir.to_path_buf(),
                offending_entry: name.to_string_lossy().to_string(),
            });
        }
    }
    // No collisions — execute every move, tracking each on the
    // rollback guard so a mid-loop failure unwinds previously-moved
    // entries.
    for src in entries {
        let Some(name) = src.file_name().map(|n| n.to_os_string()) else {
            continue;
        };
        let dst = to_dir.join(&name);
        fs::rename(&src, &dst).map_err(|e| ComposeError::Io {
            path: dst.clone(),
            source: e,
        })?;
        rollback.track_entry_rename(src, dst);
    }
    // Drop OLD — best-effort; we surface an Io error if it fails so
    // operators see the dangling directory. The OLD dir should be
    // empty by now (every entry got moved above).
    fs::remove_dir_all(from_dir).map_err(|e| ComposeError::Io {
        path: from_dir.to_path_buf(),
        source: e,
    })
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

/// Per Codex round-2 B-9 — relabel any OLD-bucket snapshot under its
/// renamed-to label BEFORE the differ runs.
///
/// Why: an `#[app(renamed_from = "old")]` annotation tells compose
/// that the app's logical label changed but its physical schema did
/// not. The pre-rename snapshot was keyed under `BucketKey { app:
/// "old", .. }`; the new model inventory keys the same tables under
/// `BucketKey { app: "new", .. }`. If the differ sees both keys it
/// emits `DropTable` on OLD and `AddTable` on NEW for every model in
/// the bucket — escalating the rename to a destructive classification
/// that wrongly demands `--allow-destructive` and re-creates every
/// table from scratch.
///
/// The fix: walk `apps` for renamed-from entries and rebuild
/// `snapshots` so the OLD bucket's snapshot value lives under the NEW
/// bucket's key. The differ then sees a single bucket on both sides
/// (NEW) with identical models — no drops, no adds, just possibly
/// column-level diffs the operator legitimately introduced.
///
/// When the OLD bucket has no snapshot, this is a no-op for that
/// rename. When BOTH OLD and NEW snapshots exist (operators rarely
/// hit this — would imply a partial earlier rename) the OLD wins
/// because the post-rename schema is what the model inventory
/// reflects, and we want the differ to see the OLD schema as the
/// "before" state being moved to NEW.
fn remap_snapshots_for_renames(
    snapshots: &std::collections::BTreeMap<BucketKey, AppliedSchema>,
    apps: &[AppLifecycle],
) -> std::collections::BTreeMap<BucketKey, AppliedSchema> {
    use std::collections::BTreeMap;

    // Build a lookup from `(database, old_label) -> new_label`. Each
    // app's `renamed_from` is at most one OLD label.
    let mut rename_map: BTreeMap<(String, String), String> = BTreeMap::new();
    for app in apps {
        if let Some(old) = app.renamed_from.as_deref() {
            rename_map.insert((app.database.clone(), old.to_string()), app.label.clone());
        }
    }
    if rename_map.is_empty() {
        // Hot path — the typical compose has no renames; clone and
        // return the input untouched.
        return snapshots.clone();
    }

    let mut remapped: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
    for (key, schema) in snapshots {
        let lookup_key = (key.database.clone(), key.app.clone());
        if let Some(new_label) = rename_map.get(&lookup_key) {
            let new_key = BucketKey {
                database: key.database.clone(),
                app: new_label.clone(),
            };
            // Update the embedded `registered_apps` list too — the
            // differ inspects it for the `App move` consistency
            // check on the destination bucket.
            let mut relabeled = schema.clone();
            for entry in &mut relabeled.registered_apps {
                if entry == &key.app {
                    *entry = new_label.clone();
                }
            }
            remapped.insert(new_key, relabeled);
        } else {
            remapped.insert(key.clone(), schema.clone());
        }
    }
    remapped
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

/// Promote a tmp file to its final path, capturing any pre-existing
/// bytes into a sibling `.bak.<pid>.<n>` backup file BEFORE the
/// rename so a later failure can restore the original content.
///
/// Per Codex round-2 B-10 the prior `promote_tmp` was not
/// restoration-safe on overwrite: a `fs::rename` over an existing
/// file silently replaced the content, and the rollback path could
/// only `remove_file(final_path)` — losing the original bytes
/// entirely. The new shape:
///
/// 1. If `final_path` already exists, copy its bytes into a sibling
///    `<final>.bak.<pid>.<counter>` backup. The counter is per-
///    process atomic so two simultaneous promotes never collide.
/// 2. Rename `tmp` over `final_path`.
/// 3. Return the backup path so the caller can hand it to the
///    [`WriteRollback`] guard for restoration on failure.
///
/// Returns `Ok(None)` when no prior file existed at `final_path`
/// (fresh create — nothing to back up). Returns `Ok(Some(path))` when
/// a backup was captured. Returns `Err` only if either I/O step
/// fails; in that case any partial backup is removed before
/// surfacing the error so the workspace is left clean.
fn promote_tmp_with_backup(tmp: &Path, final_path: &Path) -> Result<Option<PathBuf>, ComposeError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let backup_path = if final_path.exists() {
        let pid = std::process::id();
        let n = BACKUP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut name = final_path
            .file_name()
            .map(|f| f.to_os_string())
            .unwrap_or_default();
        name.push(format!(".bak.{pid}.{n}"));
        let backup = final_path.with_file_name(name);
        // Copy preserves the original bytes regardless of whether the
        // tmp's overwrite succeeds. We use `fs::copy` rather than
        // `fs::rename` because we want both files to coexist briefly
        // (the tmp will land on `final_path` next) and `rename` would
        // make the original disappear.
        fs::copy(final_path, &backup).map_err(|e| ComposeError::Io {
            path: backup.clone(),
            source: e,
        })?;
        Some(backup)
    } else {
        None
    };
    if let Err(e) = fs::rename(tmp, final_path) {
        // Promote failed — remove the just-captured backup so the
        // workspace is clean for the rollback guard's tmp cleanup
        // pass.
        if let Some(b) = backup_path {
            let _ = fs::remove_file(&b);
        }
        return Err(ComposeError::Io {
            path: final_path.to_path_buf(),
            source: e,
        });
    }
    Ok(backup_path)
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
                    generated: None,
                    identity: None,
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
                exclusion_constraints: Vec::new(),
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            generated: None,
            identity: None,
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
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

    // ── Codex round-1 fixup regression coverage ──────────────────────────

    /// Codex B-4 — D011 fires when a tombstoned app has zero current
    /// models but the snapshot still carries schema state to drop.
    /// Prior to the fix the `!s.models.is_empty()` guard skipped this
    /// path and the operator only saw the generic destructive
    /// classification error.
    #[test]
    fn b4_d011_fires_when_models_empty_but_snapshot_has_state() {
        let work = temp_workspace("b4_zero_model_d011");
        let guard = lock_for(&work);
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        // Models has the bucket entry but ZERO models inside.
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), empty_snapshot(&bucket));
        // Snapshot HAS state — a `widgets` table that the tombstone
        // would drop.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), snapshot_with_widgets(&bucket));
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
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("must surface D011");
        match err {
            ComposeError::TombstonedAppRequiresAllowDestructive { text, .. } => {
                assert!(text.contains("D011"), "must surface D011 token: {text}");
                assert!(text.contains("billing"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex B-3 — second compose with the SAME inputs but a hand
    /// edit to the up SQL file refuses with D013 (no
    /// `--force-overwrite`). With `force_overwrite = true` the same
    /// scenario succeeds and the edits are discarded.
    #[test]
    fn b3_d013_refuses_to_overwrite_hand_edited_migration() {
        let work = temp_workspace("b3_hand_edit");
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
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first");
        let up_path = r1.composed_buckets[0].up_sql_path.clone();
        // Operator hand-edits the up SQL.
        let original = fs::read_to_string(&up_path).unwrap();
        let edited = original.clone() + "\n-- operator hand-edit\n";
        fs::write(&up_path, &edited).unwrap();

        // Second compose without --force-overwrite must refuse.
        let req2 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("must refuse");
        match err {
            ComposeError::HandEditedMigrationWouldBeOverwritten { text, path, .. } => {
                // Codex round-3 B-3 — pin the FULL D013 diagnostic
                // wording so a future regression on any phrase fails
                // loudly. Frozen by `compose.rs:987-991`.
                assert!(
                    text.starts_with("D013:"),
                    "must start with D013 prefix: {text}"
                );
                assert!(
                    text.contains("hand-edited migration would be overwritten"),
                    "must carry the canonical phrase: {text}"
                );
                assert!(
                    text.contains("(up side)"),
                    "side label must read \"(up side)\" verbatim: {text}"
                );
                assert!(
                    text.contains("pass --force-overwrite"),
                    "must instruct the operator to use --force-overwrite: {text}"
                );
                assert!(
                    text.contains(&path.display().to_string()),
                    "must include the offending path: {text}"
                );
                assert_eq!(path, up_path, "path must be the up file");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // The hand-edited file is preserved on disk.
        let after_refusal = fs::read_to_string(&up_path).unwrap();
        assert_eq!(after_refusal, edited, "must not have been clobbered");

        // Third compose WITH --force-overwrite succeeds and discards
        // the edits.
        let req3 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            force_overwrite: true,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        compose(req3).expect("force-overwrite succeeds");
        let after_force = fs::read_to_string(&up_path).unwrap();
        assert_eq!(
            after_force, original,
            "force-overwrite must restore canonical SQL"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex B-5 / B-9 — round-trip rename app. Compose with
    /// `renamed_from = "oldname"` on the new bucket must:
    ///
    ///   1. Emit `UPDATE djogi_schema_migrations SET app_label =
    ///      'newname' WHERE app_label = 'oldname';` into the up SQL.
    ///   2. Emit the inverse UPDATE into the down SQL.
    ///   3. Move `migrations/main/oldname/` → `migrations/main/newname/`
    ///      on disk.
    ///   4. Per Codex round-2 B-9: succeed WITHOUT
    ///      `--allow-destructive`. The on-disk SQL tables don't move
    ///      when an app renames; `remap_snapshots_for_renames`
    ///      relabels the OLD-bucket snapshot under NEW before diffing
    ///      so no DropTable / AddTable pair appears, and the
    ///      classification stays metadata-only.
    ///   5. Per Codex round-2 B-9: the SQL must NOT carry a DROP
    ///      TABLE for the renamed-from bucket's tables — they aren't
    ///      being dropped.
    #[test]
    fn b5_rename_app_emits_ledger_update_and_renames_folder() {
        let work = temp_workspace("b5_rename_round_trip");
        let guard = lock_for(&work);
        let old_bucket = BucketKey {
            database: "main".into(),
            app: "oldname".into(),
        };
        let new_bucket = BucketKey {
            database: "main".into(),
            app: "newname".into(),
        };
        // Pre-populate the OLD app's directory with a fake prior
        // artifact so the post-rename folder existence is verifiable.
        let old_dir = bucket_dir(&work, &old_bucket);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("V20260101010101__init.sql"), "-- init").unwrap();
        // Save the snapshot at the old bucket (Codex B-1's CLI side
        // reads this; the lib-side test passes it through `snapshots`).
        let mut snapshots = BTreeMap::new();
        snapshots.insert(old_bucket.clone(), snapshot_with_widgets(&old_bucket));
        let mut models = BTreeMap::new();
        // Same shape under the new app — purely a rename.
        models.insert(new_bucket.clone(), snapshot_with_widgets(&new_bucket));
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
            // Codex round-2 B-9: pure rename must NOT require the
            // destructive opt-in.
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("compose");
        let dest = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == new_bucket)
            .expect("destination composed");
        let up = fs::read_to_string(&dest.up_sql_path).unwrap();
        let down = fs::read_to_string(&dest.down_sql_path).unwrap();
        // 1. UPDATE goes forward in up.
        assert!(
            up.contains("UPDATE djogi_schema_migrations"),
            "up must carry the ledger UPDATE: {up}"
        );
        assert!(up.contains("'newname'") && up.contains("'oldname'"));
        // 2. UPDATE reverses in down.
        assert!(
            down.contains("UPDATE djogi_schema_migrations"),
            "down must carry the inverse UPDATE: {down}"
        );
        // 3. Folder renamed.
        let new_dir = bucket_dir(&work, &new_bucket);
        assert!(new_dir.exists(), "new bucket dir must exist");
        assert!(
            !old_dir.exists(),
            "old bucket dir must have been renamed away"
        );
        // The pre-existing artifact was moved over.
        assert!(new_dir.join("V20260101010101__init.sql").exists());
        // 5. Codex round-2 B-9: the up SQL must NOT carry a DROP
        // TABLE for `widgets` — the table isn't being dropped, just
        // re-labelled at the app boundary.
        assert!(
            !up.contains("DROP TABLE \"widgets\""),
            "rename must not emit DROP TABLE for widgets: {up}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex B-7 — pending JSON with future `format_version` surfaces
    /// `UnsupportedFormatVersion` from [`parse_pending_bytes`] BEFORE
    /// the structural deserialize trips on extra fields. The
    /// production build.rs reader mirrors this peek pattern (see
    /// `b7_pending_format_version_peek_present` in the agreement
    /// integration test).
    #[test]
    fn b7_pending_format_version_peek_rejects_future_version() {
        let blob = r#"{
            "format_version": "2",
            "bucket_database": "main",
            "bucket_app": "billing",
            "version": "V20260425010203__add_invoices",
            "slug": "add_invoices",
            "model_snapshot": {
                "djogi_version": "0.2.0",
                "enums": {},
                "format_version": "1",
                "generated_at": "2027-01-01T00:00:00Z",
                "indexes": [],
                "models": {},
                "registered_apps": []
            },
            "checksum_up": "V1:0000000000000000000000000000000000000000000000000000000000000000",
            "checksum_down": null,
            "composed_at": "2026-04-25T01:02:03Z",
            "future_field_added_in_v2": "garbage"
        }"#;
        let err = parse_pending_bytes(blob.as_bytes(), None).expect_err("must fail");
        match err {
            PendingLoadError::UnsupportedFormatVersion {
                found, expected, ..
            } => {
                assert_eq!(found, "2");
                assert_eq!(expected, "1");
            }
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    /// Round-trip on a well-formed pending JSON — the loader accepts
    /// the canonical shape produced by `compose` itself.
    #[test]
    fn b7_pending_loader_accepts_current_format_version() {
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
        let parsed = parse_pending_bytes(&bytes, None).expect("loader accepts canonical shape");
        assert_eq!(parsed, plan);
    }

    /// Codex B-2 — rollback guard removes ALL staged tmp files when
    /// any rename in the dance fails. We simulate this by pre-creating
    /// the down_path as a directory (which makes the down rename fail
    /// with `IsADirectory`); the guard must remove the up tmp, the
    /// down tmp, and the pending tmp, plus roll back the up rename.
    #[test]
    fn b2_rollback_cleans_all_tmps_on_rename_failure() {
        let work = temp_workspace("b2_rollback");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));
        let now = at(2026, 4, 25, 1, 2, 3);
        // Pre-create the down_path as a non-empty directory so
        // `fs::rename(<file>, <dir>)` fails. We compute the path the
        // same way compose does.
        let prefix = version_prefix(now);
        let version = version_id(&prefix, &sanitize_slug("add widgets"));
        let down_filename_str = down_filename(&version);
        let bucket_directory = bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_directory).unwrap();
        let blocked_down = bucket_directory.join(&down_filename_str);
        fs::create_dir_all(&blocked_down).unwrap();
        // Drop a sentinel so removing the directory would matter.
        fs::write(blocked_down.join("sentinel"), b"keep").unwrap();

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("rename must fail");
        assert!(matches!(err, ComposeError::Io { .. }));

        // Now verify the workspace is clean: zero `<*>.tmp.<pid>`
        // files anywhere, and the up SQL was rolled back. The
        // pre-existing blocking directory is intentionally untouched.
        let mut tmp_files = Vec::new();
        if let Ok(entries) = fs::read_dir(&bucket_directory) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(".tmp.") {
                    tmp_files.push(name);
                }
            }
        }
        assert!(
            tmp_files.is_empty(),
            "no .tmp.<pid> file should remain: {tmp_files:?}"
        );
        // Up SQL must NOT exist (the up rename had succeeded but the
        // guard rolled it back).
        let up_path = bucket_directory.join(up_filename(&version));
        assert!(!up_path.exists(), "up SQL must have been rolled back");
        // Pending JSON also rolled back.
        let pending_path = pending_json_path(&work, &bucket);
        assert!(
            !pending_path.exists(),
            "pending JSON must have been rolled back"
        );
        // Sentinel inside the blocking directory is preserved.
        assert!(blocked_down.join("sentinel").exists());
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-10 — `WriteRollback` must restore original
    /// bytes when a tmp was promoted OVER an existing file. We
    /// simulate a mid-sequence failure by:
    ///
    /// 1. Pre-creating the up SQL file with content `"old"` (so the
    ///    up promote is an OVERWRITE, not a fresh create).
    /// 2. Pre-creating the down_path as a directory so the down
    ///    promote fails. The up promote has already succeeded by
    ///    that point, so its rollback path runs.
    ///
    /// Asserts:
    /// - tmp files cleaned up (B-2 contract still holds).
    /// - The up file's content is still `"old"` (restored from
    ///   backup, NOT the freshly-emitted bytes).
    /// - No `.bak.<pid>.<n>` sibling files remain on disk (the
    ///   rollback's restore step renames the backup back over the
    ///   final path; no backup file is left behind).
    #[test]
    fn b10_rollback_restores_original_bytes_on_overwrite_failure() {
        let work = temp_workspace("b10_overwrite_restore");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));
        let now = at(2026, 4, 25, 1, 2, 3);
        let prefix = version_prefix(now);
        let version = version_id(&prefix, &sanitize_slug("add widgets"));
        let bucket_directory = bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_directory).unwrap();
        // Pre-existing up SQL — operator's prior content. The
        // promote will overwrite this; the rollback must restore it.
        let up_path = bucket_directory.join(up_filename(&version));
        fs::write(&up_path, b"old up content").unwrap();
        // Block the down promote so the sequence fails after the up
        // promote has already overwritten the existing up file.
        let blocked_down = bucket_directory.join(down_filename(&version));
        fs::create_dir_all(&blocked_down).unwrap();

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            // Force-overwrite is required because the up file already
            // exists with non-canonical bytes (otherwise the D013
            // hand-edit guard fires before any promote happens).
            force_overwrite: true,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("down promote must fail");
        assert!(matches!(err, ComposeError::Io { .. }));

        // (a) tmp files cleaned up.
        let mut tmp_files: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&bucket_directory) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(".tmp.") {
                    tmp_files.push(name);
                }
            }
        }
        assert!(
            tmp_files.is_empty(),
            ".tmp.<pid> files must be cleaned: {tmp_files:?}"
        );

        // (b) up file's content is still the original `"old up content"`.
        let after = fs::read_to_string(&up_path).expect("up still exists");
        assert_eq!(
            after, "old up content",
            "rollback must restore original up bytes from the backup"
        );

        // (c) No `.bak.<pid>.<n>` files remain anywhere in the
        // bucket directory.
        let mut bak_files: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&bucket_directory) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(".bak.") {
                    bak_files.push(name);
                }
            }
        }
        assert!(
            bak_files.is_empty(),
            "backup files must be cleaned after restore: {bak_files:?}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-3 B-10 — `WriteRollback` must restore BOTH the up
    /// and the down bytes when a mid-sequence failure occurs after
    /// MULTIPLE promotes have already overwritten existing files.
    ///
    /// The original B-10 test (above) exercises a single restore point
    /// — the down promote fails so only the up rollback is tested.
    /// This sibling test stresses the LIFO unwind in
    /// [`WriteRollback::drop`]: it forces the failure at the THIRD
    /// promote (pending JSON), so up + down promotes have already
    /// captured backups and the rollback must restore each in reverse
    /// order.
    ///
    /// Strategy:
    ///
    /// 1. Pre-create up SQL with "operator up content".
    /// 2. Pre-create down SQL with "operator down content".
    /// 3. Block the pending JSON promote by creating its target as a
    ///    NON-EMPTY directory (so `fs::rename(<file>, <non-empty-dir>)`
    ///    fails with a kernel-level error). The `pending_path` lives
    ///    under `target/djogi_pending/<db>/<app>.json` — a different
    ///    parent from up/down — so blocking it does not interfere with
    ///    the bucket directory writes.
    ///
    /// Asserts:
    /// - The error variant matches `ComposeError::Io { .. }`.
    /// - BOTH up and down files are restored to their original
    ///   operator content (LIFO order: down restored before up; the
    ///   final on-disk state must be identical to the pre-compose
    ///   state).
    /// - No `.tmp.<pid>.<n>` or `.bak.<pid>.<n>` siblings remain.
    #[test]
    fn b10_rollback_restores_multi_promote_lifo_order() {
        let work = temp_workspace("b10_multi_promote_lifo");
        let guard = lock_for(&work);
        let bucket = global_bucket();
        let mut models = BTreeMap::new();
        models.insert(bucket.clone(), snapshot_with_widgets(&bucket));
        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));
        let now = at(2026, 4, 25, 1, 2, 3);
        let prefix = version_prefix(now);
        let version = version_id(&prefix, &sanitize_slug("add widgets"));
        let bucket_directory = bucket_dir(&work, &bucket);
        fs::create_dir_all(&bucket_directory).unwrap();

        // (1) + (2) — pre-existing operator content on BOTH SQL files.
        // Each promote will overwrite these; the rollback must restore
        // each one back to its original bytes via the LIFO unwind.
        let up_path = bucket_directory.join(up_filename(&version));
        let down_path = bucket_directory.join(down_filename(&version));
        let original_up = b"operator up content";
        let original_down = b"operator down content";
        fs::write(&up_path, original_up).unwrap();
        fs::write(&down_path, original_down).unwrap();

        // (3) — block the THIRD promote (pending JSON) by pre-creating
        // its destination as a non-empty directory. The pending path
        // lives under `target/djogi_pending/<db>/<app>.json` so we
        // need to fabricate the parent and the colliding directory
        // ourselves.
        let pending_path = pending_json_path(&work, &bucket);
        if let Some(parent) = pending_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::create_dir_all(&pending_path).unwrap();
        fs::write(pending_path.join("sentinel"), b"keep").unwrap();

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            // Force-overwrite required: both up and down were edited
            // (have non-canonical content), so D013 would otherwise
            // fire BEFORE any promote happens — and we need the
            // promotes to run so the multi-promote rollback path is
            // exercised.
            force_overwrite: true,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("pending promote must fail");
        assert!(
            matches!(err, ComposeError::Io { .. }),
            "must surface a typed I/O error: {err:?}"
        );

        // (a) BOTH up and down files restored to their original
        //     operator content. The LIFO unwind in
        //     `WriteRollback::drop` runs the down restore first, then
        //     the up restore — but we only observe the final state,
        //     which must match the pre-compose state byte-for-byte.
        let after_up = fs::read(&up_path).expect("up file still present");
        assert_eq!(
            after_up.as_slice(),
            original_up,
            "up file must be restored to original operator content"
        );
        let after_down = fs::read(&down_path).expect("down file still present");
        assert_eq!(
            after_down.as_slice(),
            original_down,
            "down file must be restored to original operator content"
        );

        // (b) No `.tmp.<pid>.<n>` files remain in the bucket directory.
        let mut tmp_files: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&bucket_directory) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(".tmp.") {
                    tmp_files.push(name);
                }
            }
        }
        assert!(
            tmp_files.is_empty(),
            ".tmp.<pid> files must be cleaned: {tmp_files:?}"
        );

        // (c) No `.bak.<pid>.<n>` files remain anywhere in the bucket
        //     directory. The LIFO restore renames each backup back
        //     over its final path, leaving zero backup siblings.
        let mut bak_files: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&bucket_directory) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(".bak.") {
                    bak_files.push(name);
                }
            }
        }
        assert!(
            bak_files.is_empty(),
            "backup files must be cleaned after restore: {bak_files:?}"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-3 — D013 also fires when ONLY the down SQL was
    /// hand-edited. The original B-3 test only covered the up side;
    /// round-2 caught the down side as silently overwriteable.
    #[test]
    fn b3_round2_d013_fires_on_down_only_hand_edit() {
        let work = temp_workspace("b3r2_down_only");
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
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first compose");
        let down_path = r1.composed_buckets[0].down_sql_path.clone();
        let original_down = fs::read_to_string(&down_path).unwrap();
        let edited_down = original_down.clone() + "\n-- operator hand-edit on down only\n";
        fs::write(&down_path, &edited_down).unwrap();

        let req2 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("down hand-edit must refuse");
        match err {
            ComposeError::HandEditedMigrationWouldBeOverwritten { text, path, .. } => {
                // Codex round-3 B-3 — pin the FULL D013 diagnostic
                // wording (down side variant). Frozen format string
                // lives at `compose.rs:987-991`.
                assert!(
                    text.starts_with("D013:"),
                    "must start with D013 prefix: {text}"
                );
                assert!(
                    text.contains("hand-edited migration would be overwritten"),
                    "must carry the canonical phrase: {text}"
                );
                assert!(
                    text.contains("(down side)"),
                    "side label must read \"(down side)\" verbatim: {text}"
                );
                assert!(
                    text.contains("pass --force-overwrite"),
                    "must instruct the operator to use --force-overwrite: {text}"
                );
                assert!(
                    text.contains(&path.display().to_string()),
                    "must include the offending path: {text}"
                );
                assert_eq!(path, down_path, "path must be the down file");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // The hand-edited down file is preserved on disk.
        let after = fs::read_to_string(&down_path).unwrap();
        assert_eq!(after, edited_down);
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-3 — D013 fires when BOTH up and down were
    /// edited. The diagnostic surfaces both via the side label.
    #[test]
    fn b3_round2_d013_fires_on_both_sides_hand_edit() {
        let work = temp_workspace("b3r2_both");
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
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first compose");
        let up_path = r1.composed_buckets[0].up_sql_path.clone();
        let down_path = r1.composed_buckets[0].down_sql_path.clone();
        fs::write(&up_path, b"-- hand edit up\n").unwrap();
        fs::write(&down_path, b"-- hand edit down\n").unwrap();

        let req2 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "add widgets",
            allow_destructive: false,
            force_overwrite: false,
            now,
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("both-side edit must refuse");
        match err {
            ComposeError::HandEditedMigrationWouldBeOverwritten { text, path, .. } => {
                // Codex round-3 B-3 — pin the FULL D013 diagnostic
                // wording (both-sides variant). The reporter favours
                // the up path when both sides were edited (operator
                // typically inspects up first); see
                // `compose.rs:981-985`.
                assert!(
                    text.starts_with("D013:"),
                    "must start with D013 prefix: {text}"
                );
                assert!(
                    text.contains("hand-edited migration would be overwritten"),
                    "must carry the canonical phrase: {text}"
                );
                assert!(
                    text.contains("(up and down side)"),
                    "side label must read \"(up and down side)\" verbatim: {text}"
                );
                assert!(
                    text.contains("pass --force-overwrite"),
                    "must instruct the operator to use --force-overwrite: {text}"
                );
                assert!(
                    text.contains(&path.display().to_string()),
                    "must include the offending path: {text}"
                );
                assert_eq!(path, up_path, "both-edited reports the up path");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-9 — rename app with multiple existing tables
    /// must succeed WITHOUT `--allow-destructive`. This guards the
    /// snapshot-key remap step in `remap_snapshots_for_renames`: if
    /// the remap regresses, the differ would emit DropTable for each
    /// of the OLD bucket's three tables and the test would fail with
    /// `DestructiveRequiresAllowDestructive`.
    #[test]
    fn b9_rename_app_with_three_tables_no_allow_destructive() {
        let work = temp_workspace("b9_rename_three_tables");
        let guard = lock_for(&work);
        let old_bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let new_bucket = BucketKey {
            database: "main".into(),
            app: "invoicing".into(),
        };
        // Three tables on the OLD side, three IDENTICAL tables on the
        // NEW side. Only the bucket label changed.
        fn three_tables(bucket: &BucketKey) -> AppliedSchema {
            let mut s = AppliedSchema {
                djogi_version: env!("CARGO_PKG_VERSION").to_string(),
                enums: BTreeMap::new(),
                format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
                generated_at: "2026-04-25T00:00:00Z".to_string(),
                indexes: Vec::new(),
                models: BTreeMap::new(),
                registered_apps: vec![bucket.app.clone()],
            };
            for name in ["invoices", "customers", "line_items"] {
                s.models.insert(
                    name.to_string(),
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
                            generated: None,
                            identity: None,
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
                        exclusion_constraints: Vec::new(),
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
                        table: name.to_string(),
                        tenant_key: None,
                    },
                );
            }
            s
        }
        let mut snapshots = BTreeMap::new();
        snapshots.insert(old_bucket.clone(), three_tables(&old_bucket));
        let mut models = BTreeMap::new();
        models.insert(new_bucket.clone(), three_tables(&new_bucket));
        let app = AppLifecycle {
            label: "invoicing".to_string(),
            database: "main".to_string(),
            renamed_from: Some("billing".to_string()),
            tombstone: false,
        };
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: std::slice::from_ref(&app),
            name: "rename invoicing",
            // Crucial: NO allow_destructive flag.
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("rename without --allow-destructive must succeed");
        let dest = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == new_bucket)
            .expect("destination bucket composed");
        let up = fs::read_to_string(&dest.up_sql_path).unwrap();
        // No DropTable for any of the three table names.
        for name in ["invoices", "customers", "line_items"] {
            let drop_text = format!("DROP TABLE \"{name}\"");
            assert!(
                !up.contains(&drop_text),
                "rename must not emit {drop_text} (B-9): {up}"
            );
        }
        // The RenameApp ledger UPDATE is still there.
        assert!(up.contains("UPDATE djogi_schema_migrations"));
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-11 — `rename_old_bucket_folder` refuses
    /// fail-fast when the destination directory already contains an
    /// entry colliding with the OLD directory's content. The prior
    /// shape silently skipped collisions (dropping the OLD entry); the
    /// new shape returns a typed `FolderRenameTargetCollision` error
    /// before any move happens.
    #[test]
    fn b11_folder_rename_collision_refuses_fail_fast() {
        let work = temp_workspace("b11_collision");
        let guard = lock_for(&work);
        let old_bucket = BucketKey {
            database: "main".into(),
            app: "oldname".into(),
        };
        let new_bucket = BucketKey {
            database: "main".into(),
            app: "newname".into(),
        };
        let old_dir = bucket_dir(&work, &old_bucket);
        let new_dir = bucket_dir(&work, &new_bucket);
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&new_dir).unwrap();
        // Both directories contain a file of the SAME name with
        // DIFFERENT content — a collision the prior merge loop would
        // silently swallow.
        fs::write(old_dir.join("V20260101010101__init.sql"), "from-old").unwrap();
        fs::write(new_dir.join("V20260101010101__init.sql"), "from-new").unwrap();
        let mut snapshots = BTreeMap::new();
        snapshots.insert(old_bucket.clone(), snapshot_with_widgets(&old_bucket));
        let mut models = BTreeMap::new();
        models.insert(new_bucket.clone(), snapshot_with_widgets(&new_bucket));
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
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Track 0: existing compose unit tests target the
            // delta-based write/rollback machinery in isolation. The
            // Phase 0 auto-emit is exercised by dedicated integration
            // + unit tests; opt out here so the per-bucket directory
            // assertions stay tight to what these tests actually
            // verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("collision must surface");
        match err {
            ComposeError::FolderRenameTargetCollision {
                offending_entry, ..
            } => {
                assert_eq!(offending_entry, "V20260101010101__init.sql");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // The pre-existing files are left untouched (no partial
        // merge state).
        assert_eq!(
            fs::read_to_string(old_dir.join("V20260101010101__init.sql")).unwrap(),
            "from-old"
        );
        assert_eq!(
            fs::read_to_string(new_dir.join("V20260101010101__init.sql")).unwrap(),
            "from-new"
        );
        let _ = fs::remove_dir_all(&work);
    }

    /// Codex round-2 B-8 — both `classify_bucket` and
    /// `classify_bucket_with_pending` route through the same
    /// underlying logic. The convenience wrapper supplies `None` for
    /// `pending_version` so the message uses the `<unknown>`
    /// placeholder; production callers go through the with-pending
    /// path.
    #[test]
    fn b8_classify_bucket_routes_through_with_pending() {
        // We exercise both entry points and assert they agree on the
        // None-version case (the classify_bucket convenience wrapper
        // forwards directly to classify_bucket_with_pending(.., None)).
        use super::super::build_match::{classify_bucket, classify_bucket_with_pending};
        use super::super::schema::SNAPSHOT_FORMAT_VERSION;
        let bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let drifted = AppliedSchema {
            djogi_version: "9.9.9".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: Vec::new(),
        };
        let synced = AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            ..drifted.clone()
        };
        let via_wrapper = classify_bucket(&bucket, Some(&drifted), Some(&drifted), Some(&synced))
            .expect("must produce diagnostic");
        let via_direct = classify_bucket_with_pending(
            &bucket,
            Some(&drifted),
            Some(&drifted),
            Some(&synced),
            None,
        )
        .expect("must produce diagnostic");
        assert_eq!(via_wrapper, via_direct);
    }

    /// Codex round-3 B-11 (testing-gap acknowledgement) — the
    /// `WriteRollback.entry_renames` queue exists so a mid-loop
    /// failure during the post-compose folder merge unwinds every
    /// already-moved entry. In practice the pre-flight collision scan
    /// in `rename_old_bucket_folder` (compose.rs:1115-1127) catches
    /// every deterministically-reachable conflict before any entry
    /// move runs — so the rollback path is unreachable from a unit
    /// test harness without monkey-patching `fs::rename` to fail
    /// mid-iteration.
    ///
    /// This test pins that observation: it constructs two distinct
    /// collision shapes (file-vs-file and file-vs-directory) and
    /// asserts the pre-flight surfaces a typed
    /// [`ComposeError::FolderRenameTargetCollision`] BEFORE any move
    /// happens. The OLD directory is left intact (the rollback queue
    /// would be irrelevant — pre-flight pre-empted it).
    ///
    /// A non-vacuous test would require simulating a mid-loop
    /// kernel-level I/O failure (out-of-disk, permission flip between
    /// iterations, TOCTOU race), none of which are portably
    /// reproducible from a unit test. The defensive comment in
    /// `WriteRollback::drop` records this gap; a future hardening
    /// pass that removes the pre-flight (or that adds a TOCTOU race
    /// fence in production) must land a real mid-loop test alongside.
    #[test]
    fn b11_pre_flight_pre_empts_mid_loop_rollback() {
        // Shape 1 — file-vs-file collision on a single entry. The
        // pre-flight catches this (already covered by
        // `b11_folder_rename_collision_refuses_fail_fast` for the
        // single-entry case; we reproduce the assertion here so the
        // gap-acknowledgement test is self-contained).
        {
            let work = temp_workspace("b11_gap_file_file");
            let guard = lock_for(&work);
            let old_bucket = BucketKey {
                database: "main".into(),
                app: "oldname".into(),
            };
            let new_bucket = BucketKey {
                database: "main".into(),
                app: "newname".into(),
            };
            let old_dir = bucket_dir(&work, &old_bucket);
            let new_dir = bucket_dir(&work, &new_bucket);
            fs::create_dir_all(&old_dir).unwrap();
            fs::create_dir_all(&new_dir).unwrap();
            // Two entries on the OLD side; the SECOND one collides on
            // the NEW side. If the pre-flight check were ever loosened
            // to skip later entries, the first move would land and the
            // second would fail mid-loop — which is the scenario we
            // want to make unreachable. Today the pre-flight inspects
            // every entry up-front and refuses fail-fast.
            fs::write(old_dir.join("V20260101010101__a.sql"), "movable").unwrap();
            fs::write(old_dir.join("V20260101010102__b.sql"), "from-old").unwrap();
            fs::write(new_dir.join("V20260101010102__b.sql"), "from-new").unwrap();
            let mut snapshots = BTreeMap::new();
            snapshots.insert(old_bucket.clone(), snapshot_with_widgets(&old_bucket));
            let mut models = BTreeMap::new();
            models.insert(new_bucket.clone(), snapshot_with_widgets(&new_bucket));
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
                allow_destructive: false,
                force_overwrite: false,
                now: at(2026, 4, 25, 1, 2, 3),
                _guard: &guard,
                pk_flip_join_table_option: None,
                skip_phase_zero_auto_emit: true,
            };
            let err = compose(req).expect_err("collision must surface");
            match err {
                ComposeError::FolderRenameTargetCollision {
                    offending_entry, ..
                } => {
                    assert_eq!(offending_entry, "V20260101010102__b.sql");
                }
                other => panic!("wrong variant (file-vs-file): {other:?}"),
            }
            // Pre-flight pre-empted the move loop — the OLD
            // directory's MOVABLE entry must still be in OLD (not in
            // NEW). If the pre-flight ever regressed to "skip
            // colliding entries and silently keep going", this
            // assertion would fail because the first entry would have
            // been moved before the second hit the rollback path.
            assert!(
                old_dir.join("V20260101010101__a.sql").exists(),
                "movable entry must remain under OLD — pre-flight \
                 must pre-empt the entire merge loop"
            );
            assert!(
                !new_dir.join("V20260101010101__a.sql").exists(),
                "movable entry must NOT have been promoted into NEW"
            );
            let _ = fs::remove_dir_all(&work);
        }

        // Shape 2 — file-vs-directory collision. The OLD entry is a
        // file; the NEW side has a DIRECTORY at the same name. The
        // pre-flight uses `Path::exists()` which returns true for
        // both files and directories, so the collision is caught
        // before any rename attempt.
        {
            let work = temp_workspace("b11_gap_file_dir");
            let guard = lock_for(&work);
            let old_bucket = BucketKey {
                database: "main".into(),
                app: "oldname".into(),
            };
            let new_bucket = BucketKey {
                database: "main".into(),
                app: "newname".into(),
            };
            let old_dir = bucket_dir(&work, &old_bucket);
            let new_dir = bucket_dir(&work, &new_bucket);
            fs::create_dir_all(&old_dir).unwrap();
            fs::create_dir_all(&new_dir).unwrap();
            // OLD has a file at `V20260101010101__init.sql`. NEW has
            // a DIRECTORY at the same path. Without the pre-flight,
            // `fs::rename(<file>, <existing-dir>)` would fail
            // mid-loop with EISDIR.
            fs::write(old_dir.join("V20260101010101__init.sql"), "movable").unwrap();
            fs::create_dir_all(new_dir.join("V20260101010101__init.sql")).unwrap();
            fs::write(
                new_dir.join("V20260101010101__init.sql").join("sentinel"),
                b"keep",
            )
            .unwrap();
            let mut snapshots = BTreeMap::new();
            snapshots.insert(old_bucket.clone(), snapshot_with_widgets(&old_bucket));
            let mut models = BTreeMap::new();
            models.insert(new_bucket.clone(), snapshot_with_widgets(&new_bucket));
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
                allow_destructive: false,
                force_overwrite: false,
                now: at(2026, 4, 25, 1, 2, 3),
                _guard: &guard,
                pk_flip_join_table_option: None,
                skip_phase_zero_auto_emit: true,
            };
            let err = compose(req).expect_err("file-vs-dir collision must surface");
            match err {
                ComposeError::FolderRenameTargetCollision {
                    offending_entry, ..
                } => {
                    assert_eq!(offending_entry, "V20260101010101__init.sql");
                }
                other => panic!("wrong variant (file-vs-dir): {other:?}"),
            }
            // Sentinel inside the blocking directory survives — the
            // rollback never ran because pre-flight pre-empted it.
            assert!(
                new_dir
                    .join("V20260101010101__init.sql")
                    .join("sentinel")
                    .exists(),
                "blocking directory's contents must be preserved"
            );
            let _ = fs::remove_dir_all(&work);
        }
    }

    /// Codex round-3 B-9 — `remap_snapshots_for_renames` must rewrite
    /// the OLD bucket key AND the embedded `registered_apps` list on
    /// the relabeled snapshot, while leaving every other bucket in the
    /// input map untouched.
    ///
    /// The differ inspects `registered_apps` on the destination bucket
    /// for an "app move" consistency check. If the relabel only
    /// rewrote the BTreeMap key but left the embedded list pointing at
    /// the OLD label, the differ would see a mismatch where the new
    /// bucket's snapshot does not list itself as a registered app —
    /// regressing the rename path silently.
    #[test]
    fn b9_remap_relabels_registered_apps_field() {
        // BEFORE: bucket map keyed under the OLD app label "billing".
        // The snapshot's `registered_apps` lists three apps including
        // "billing". A second untouched bucket ("audit") confirms the
        // remap leaves unrelated entries alone.
        let old_billing_bucket = BucketKey {
            database: "main".into(),
            app: "billing".into(),
        };
        let audit_bucket = BucketKey {
            database: "main".into(),
            app: "audit".into(),
        };
        let mut before_billing = empty_snapshot(&old_billing_bucket);
        before_billing.registered_apps =
            vec!["".to_string(), "billing".to_string(), "users".to_string()];
        let mut before_audit = empty_snapshot(&audit_bucket);
        before_audit.registered_apps = vec!["audit".to_string()];
        let mut before: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
        before.insert(old_billing_bucket.clone(), before_billing.clone());
        before.insert(audit_bucket.clone(), before_audit.clone());

        // AppLifecycle entry models the rename `billing -> invoicing`.
        let apps = [AppLifecycle {
            label: "invoicing".to_string(),
            database: "main".to_string(),
            renamed_from: Some("billing".to_string()),
            tombstone: false,
        }];

        let after = remap_snapshots_for_renames(&before, &apps);

        // (a) The OLD billing bucket key has been rewritten to NEW
        //     under the same database; the OLD key no longer exists.
        let new_billing_bucket = BucketKey {
            database: "main".into(),
            app: "invoicing".into(),
        };
        assert!(
            after.contains_key(&new_billing_bucket),
            "remap must produce the new bucket key (main, invoicing)"
        );
        assert!(
            !after.contains_key(&old_billing_bucket),
            "remap must drop the old bucket key (main, billing)"
        );

        // (b) The relabeled snapshot's `registered_apps` field
        //     contains "invoicing" and does NOT contain "billing".
        let relabeled = &after[&new_billing_bucket];
        assert!(
            relabeled.registered_apps.iter().any(|s| s == "invoicing"),
            "registered_apps must contain new label \"invoicing\": {:?}",
            relabeled.registered_apps
        );
        assert!(
            !relabeled.registered_apps.iter().any(|s| s == "billing"),
            "registered_apps must drop old label \"billing\": {:?}",
            relabeled.registered_apps
        );
        // Sibling entries ("" global and "users") are preserved
        // verbatim — only the renamed-from entry was rewritten.
        assert!(relabeled.registered_apps.iter().any(|s| s.is_empty()));
        assert!(relabeled.registered_apps.iter().any(|s| s == "users"));

        // (c) The unrelated `audit` bucket is unchanged in both key
        //     and value (including its registered_apps list).
        let after_audit = after.get(&audit_bucket).expect("audit untouched");
        assert_eq!(*after_audit, before_audit);
    }
}
