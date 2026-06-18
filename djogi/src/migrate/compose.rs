//! `migrations compose` orchestrator — central entry point.
//! Compose translates the descriptor inventory + the last-applied
//! snapshot into one new pair of files per drifted bucket:
//! 1. The committed migration SQL pair under
//!    `migrations/<database>/<app>/<version>.sdjql` (up) +
//!    `<version>.down.sdjql` (down).
//! 2. The pending JSON at
//!    `target/djogi_pending/<database>/<app>.json` recording the
//!    composed delta + checksum (build.rs reads it as the second leg
//!    of the three-way match).
//!    The two writes are **atomic** — both succeed or neither. We write
//!    to `<final>.tmp.<pid>` siblings, fsync, then rename the SQL pair
//!    into place, then rename the pending JSON. On any rename failure
//!    the partial state is rolled back.
//! # — overwrite-on-same-slug
//! Re-running `compose --name <slug>` against the same model state
//! and snapshot overwrites both files. The same input produces
//! byte-identical output (the SQL emitter is deterministic), so the
//! overwrite is a no-op on disk modulo the rename dance. Different
//! `--name` against the same delta refuses with [`ComposeError::NothingToCompose`]
//! because the differ produces an empty operation list.
//! # / — lifecycle markers
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
//! # No regex
//! The slug derivation goes through [`super::naming::sanitize_slug`]
//! which is byte-level only.
//! # `clippy::result_large_err`
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

use super::common;
use super::diff::{Classification, SchemaDelta, SchemaOperation, diff_bucket_maps};
use super::guard::WorkspaceGuard;
use super::ledger::compute_checksum;
use super::naming::{down_filename, sanitize_slug, up_filename, version_id, version_prefix};
use super::projection::BucketKey;
use super::replay_plan::{committed_replay_plan_path, serialize_committed_replay_plan};
use super::schema::AppliedSchema;
use super::segment::{Segment, SegmentKind, plan_delta};
use super::snapshot::SnapshotError;
use super::snapshot::serialize_snapshot;
use super::sql::{OperationSql, lower_delta};
use super::target::{
    bucket_dir, pending_database_dir, pending_json_path, pending_root, snapshot_path,
};

/// One restore point captured before a tmp file was promoted onto a
/// destination that already had bytes on it.
/// `promote_tmp` overwrites the final path via `fs::rename`. Without a
/// backup of the prior bytes, a later failure in the same compose
/// sequence cannot restore the original file — the rollback only knew
/// to `remove_file(final_path)`, leaving the workspace in a half-state.
/// This struct carries both the final path (where the new bytes live
/// after a successful promote) and the backup path (where the prior
/// bytes were copied just before the rename). On commit we delete the
/// backup; on failure we restore the backup over the final path.
struct RestorePoint {
    /// The artifact's final path on disk (the post-promote location).
    final_path: PathBuf,
    /// Sibling backup file that holds the pre-overwrite bytes.
    /// `None` when no prior file existed and the promote was a fresh
    /// create rather than an overwrite — nothing to restore.
    backup_path: Option<PathBuf>,
}

/// RAII rollback guard for atomic compose writes.
/// Tracks three parallel cleanup queues:
/// 1. `tmps` — staged `<final>.tmp.<pid>` files that have been
///    written but not yet promoted. These are removed on failure.
/// 2. `restore_points` — files that have already been renamed into
///    their final location, possibly OVER an existing file. On failure
///    we restore the prior bytes (via the backup path) when one was
///    captured, otherwise we delete the freshly-promoted file. The
///    previous shape only deleted the final path on rollback, which
///    silently lost the original content for overwrite cases.
/// 3. `entry_renames` — entries that were moved from one directory to
///    another by [`rename_old_bucket_folder`]. On failure we move them
///    back. The merge loop touched many files and a mid-loop failure
///    left partial state untracked.
///    On a successful sequence the caller invokes [`commit`](Self::commit)
///    to drain every queue (and delete the backups) — the [`Drop`] impl
///    then runs as a no-op. On any failure path the guard goes out of
///    scope without `commit` being called and every tracked artifact is
///    rolled back via best-effort filesystem ops.
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
    /// `final_path` to maintain an overwrite-safe contract.
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
    /// from `to` to `from`. The merge loop must be undoable so a
    /// mid-loop failure does not leak partial state.
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
        // Best-effort cleanup. Errors are intentionally swallowed
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
        // Undo every tracked entry rename. We move each `to` back to
        // its prior `from` location.
        // This rollback path is reachable in principle (a `fs::rename`
        // call inside the merge loop could fail mid-iteration on
        // out-of-disk, EPERM, or a TOCTOU race against the pre-flight
        // check), but in practice the pre-flight collision scan in
        // `rename_old_bucket_folder` catches every deterministically-
        // reachable failure before any entry has been moved — so this
        // branch executes zero queue entries on every test run. A
        // non-vacuous test would have to simulate a mid-loop kernel-
        // level failure (permission flip between iterations, disk-full
        // on the second move, etc.) and those are not portably
        // reproducible from a unit test harness. The rollback queue is
        // kept alive defensively so a future change to the pre-flight
        // (or a TOCTOU race in production) cannot leave the workspace
        // half-merged. See `b11_pre_flight_pre_empts_mid_loop_rollback`
        // for the documented gap.
        for (from, to) in self.entry_renames.drain(..).rev() {
            let _ = fs::rename(&to, &from);
        }
    }
}

// ── Errors ─────────────────────────────────────────────────────────────────

/// Errors surfaced by [`compose`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ComposeError {
    /// The differ produced an empty operation list for every bucket
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
    /// REQ-370-16 — a committed snapshot BUCKET `(database, app)` still
    /// describes tables, but the CURRENT projection has zero models for
    /// that bucket and the app is not tombstoned. This is the
    /// linkage-error shape: the app's model crate was probably not linked
    /// into the running binary, so compose would emit `DROP TABLE` for
    /// tables that still exist. Evaluated on the POST-rename-remap
    /// snapshots, so a renamed app (models carried to the new label) does
    /// NOT trip it. Refuses EVEN with `--allow-destructive`, because the
    /// generic destructive gate only covers the default path, and the
    /// dangerous case (drop X on purpose, silently lose unlinked Y)
    /// bypasses it. Intentional whole-app removal uses `#[app(tombstone)]`.
    LinkageDropWithoutModels {
        /// App label whose snapshot bucket has tables but no current
        /// models (`""` for the synthetic global bucket).
        app_label: String,
        /// Database target the bucket belongs to.
        database: String,
        /// Pre-formatted linkage-specific diagnostic.
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
    /// removal, etc. The operator hand-writes the migration. Compose stops
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
    /// The destination pending JSON path already exists but its
    /// contents do not represent a compatible pending authority for the
    /// bucket being composed. We refuse rather than silently overwrite
    /// malformed, foreign, or legacy-Phase-0 content.
    PendingJsonWouldBeOverwritten { path: PathBuf, text: String },
    /// `rename_old_bucket_folder` would have to merge the OLD app's
    /// directory into a NEW directory that already contains conflicting
    /// entries. The old shape attempted a non-atomic merge loop; we now
    /// refuse fail-fast so the operator resolves the conflict explicitly
    /// instead of silently leaving a partial-merge state on disk.
    FolderRenameTargetCollision {
        /// Source directory (the OLD app's bucket dir).
        from: PathBuf,
        /// Destination directory whose entries collided.
        to: PathBuf,
        /// One offending entry name — included so the operator can
        /// move or delete it before re-running compose.
        offending_entry: String,
    },
    /// The differ surfaced a structured `DiffError` (e.g. a PK-flip
    /// transitive FK closure exceeded the depth contract). Compose
    /// rendered the error verbatim rather than letting the panic
    /// unwind the run.
    Diff(super::diff::DiffError),
    /// Bootstrap auto-emit failed. `migrations compose` was wired to
    /// emit a bootstrap migration before its delta-based work for any
    /// database that doesn't already have one. The wrapped error names
    /// the failing step (composition vs. filesystem write vs.
    /// pending-JSON serialize).
    PhaseZeroAutoEmit(super::bootstrap::AutoEmitError),
    /// Foreign keys between app buckets form a cycle — no bucket apply
    /// order can satisfy them. Automatic cycle-breaking is out of scope;
    /// the operator moves the mutually-referencing models into one app,
    /// or removes one direction of the reference. See #399 for
    /// operator-declared resolution design.
    CrossBucketForeignKeyCycle {
        database: String,
        chain: Vec<String>,
    },
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingToCompose => write!(
                f,
                "D012: nothing to compose — model state matches snapshot for every bucket"
            ),
            Self::TombstonedAppRequiresAllowDestructive { text, .. } => f.write_str(text),
            Self::LinkageDropWithoutModels { text, .. } => f.write_str(text),
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
            Self::PendingJsonWouldBeOverwritten { text, .. } => f.write_str(text),
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
            Self::CrossBucketForeignKeyCycle { database, chain } => {
                write!(
                    f,
                    "cross-app foreign keys in database `{database}` form a dependency cycle \
                     between apps: {chain}. No slice apply order can satisfy the cycle. Move \
                     the mutually-referencing models into one app, or remove one direction of \
                     the reference",
                    chain = chain.join(", ")
                )
            }
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
    /// the workspace lock per the file-lock contract.
    pub _guard: &'a WorkspaceGuard,
    /// Join-table cutover layout for any PK-flip group emitted by
    /// the differ. `None` defaults to
    /// [`super::diff::PkFlipJoinTableOption::OptionA`] — single
    /// mega-transaction across both parents and the join table per
    /// playbook §7. `Some(OptionB)` selects sequential per-parent
    /// flips. Production callers pass the operator's
    /// [`crate::config::MigrateConfig::pk_flip_join_table_option`]
    /// converted via
    /// [`super::diff::PkFlipJoinTableOption::from_config_char`].
    pub pk_flip_join_table_option: Option<super::diff::PkFlipJoinTableOption>,
    /// Opt out of bootstrap auto-emit.
    /// Production callers leave this `false` (the default behaviour):
    /// every database referenced in `models` ∪ `apps` that doesn't
    /// already have a bootstrap migration on disk receives one before
    /// the regular delta-based work runs.
    /// Tests that exercise compose's lower-level write / rollback
    /// machinery in isolation (no real schema, just the file dance)
    /// set this to `true` to keep the per-bucket directory free of
    /// the auto-emitted bootstrap artefacts. The skip is a test-only
    /// affordance — the CLI / production paths always go through the
    /// full auto-emit flow.
    /// Not adopter API. Setting this `true` from outside the crate
    /// bypasses and is unsupported.
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
    /// One entry per database that received a bootstrap migration
    /// during this compose run. Auto-emit wires each database so any
    /// database whose
    /// `migrations/<db>/_global_/V00000000000000__phase_zero_bootstrap.sdjql`
    /// is missing receives one before the delta-based work runs. Empty
    /// when every database already had a bootstrap migration on disk.
    pub emitted_phase_zero: Vec<super::bootstrap::EmittedPhaseZero>,
    /// Buckets whose deltas were entirely consumed by cross-bucket
    /// reconciliation (e.g. a `DropEnum` suppressed because another
    /// bucket still projects the type). For these buckets no migration
    /// SQL is written, but the current scoped model snapshot is written
    /// to `migrations/<db>/<app>/schema_snapshot.json` so that the next
    /// `build.rs` run sees no drift and does not falsely warn "run compose".
    pub converged_snapshot_buckets: Vec<BucketKey>,
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
    /// Path written for the committed replay-plan sidecar.
    pub replay_plan_path: PathBuf,
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
/// Serialised with `#[serde(deny_unknown_fields)]` so the build.rs
/// reader rejects future-shape pending files explicitly rather than
/// silently dropping unknown keys. Format-version handling lives at
/// the top level so older Djogi reading a newer pending file gets a
/// clear upgrade error rather than a generic `unknown field`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingPlan {
    /// Pending JSON format version. Currently `"2"`; see [`PENDING_FORMAT_VERSION`].
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
    /// Up-side canonical operation-fragment checksum — `V1:<sha256-hex>`.
    pub checksum_up: String,
    /// Down-side canonical operation-fragment checksum — `None` when
    /// every operation's down is a SQL-comment placeholder (every
    /// drop is lossy → no real rollback).
    pub checksum_down: Option<String>,
    /// Compose timestamp (RFC 3339 UTC, second precision).
    pub composed_at: String,

    /// App labels (same database) whose pending migrations must apply
    /// BEFORE this bucket's. Compose derives the list from cross-bucket
    /// foreign-key targets; apply orders same-version buckets with it.
    /// Sorted and deduplicated. The empty string names the global
    /// bucket. Introduced with pending format "2".
    pub depends_on: Vec<String>,
}

/// Pending-JSON format version. Bumped when the [`PendingPlan`] shape
/// changes incompatibly.
///
/// Format `"2"`: added `depends_on` field (cross-bucket FK ordering).
/// Stale format-`"1"` pending files are rejected with
/// [`PendingLoadError::UnsupportedFormatVersion`]; the operator must
/// recompose. Pending files use a stricter bump policy than snapshots
/// (`deny_unknown_fields` + version peek), so the snapshot additive-field
/// exemption does not apply.
pub const PENDING_FORMAT_VERSION: &str = "2";

/// Errors surfaced by [`parse_pending_bytes`].
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
    /// recovery message instead of a `deny_unknown_fields` shower.
    UnsupportedFormatVersion {
        found: String,
        expected: &'static str,
        path: Option<PathBuf>,
    },
}

/// Direction-aware recovery hint for an UnsupportedFormatVersion message.
/// Numeric comparison so the operator is told to recompose (stale) vs.
/// upgrade djogi (future); non-numeric versions fall back to the generic
/// upgrade hint without panicking.
fn format_version_recovery_hint(found: &str, expected: &str) -> &'static str {
    match (found.parse::<u64>().ok(), expected.parse::<u64>().ok()) {
        (Some(f), Some(e)) if f < e => {
            "re-run 'djogi migrations compose' to regenerate this pending file"
        }
        (Some(f), Some(e)) if f > e => {
            "upgrade to a newer version of djogi (or check out a newer revision)"
        }
        _ => "upgrade or check out a newer djogi",
    }
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
            } => {
                let hint = format_version_recovery_hint(found, expected);
                match path {
                    Some(p) => write!(
                        f,
                        "pending JSON format version '{found}' at {} is not supported by this Djogi (expected '{expected}'); {hint}",
                        p.display()
                    ),
                    None => write!(
                        f,
                        "pending JSON format version '{found}' is not supported by this Djogi (expected '{expected}'); {hint}"
                    ),
                }
            }
        }
    }
}

impl std::error::Error for PendingLoadError {}

/// Parse a pending JSON byte slice with a format-version peek before
/// structural deserialize.
/// Mirrors the snapshot loader's two-stage pattern: a permissive
/// `serde_json::Value` parse first to inspect the top-level
/// `format_version`, then a strict
/// [`serde(deny_unknown_fields)`]-driven structural parse. Future
/// pending-format versions surface
/// [`PendingLoadError::UnsupportedFormatVersion`] with both the found
/// and expected versions so the operator's message is actionable.
/// `path` is purely for error reporting; pass `None` when the bytes
/// come from memory.
pub fn parse_pending_bytes(
    bytes: &[u8],
    path: Option<PathBuf>,
) -> Result<PendingPlan, PendingLoadError> {
    // Phase 1 — peek at `format_version`. A future version with
    // additional fields would otherwise trip `deny_unknown_fields`
    // in phase 2 with a cryptic error.
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
    // Phase 2 — strict structural parse.
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
    let base = path
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| Path::new("."));
    let base = base.canonicalize().map_err(|e| PendingLoadError::Parse {
        path: Some(path.to_path_buf()),
        source: serde_json::Error::io(e),
    })?;
    let path = path.canonicalize().map_err(|e| PendingLoadError::Parse {
        path: Some(path.to_path_buf()),
        source: serde_json::Error::io(e),
    })?;
    if !path.starts_with(&base) {
        return Err(PendingLoadError::Parse {
            path: Some(path),
            source: serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "pending path escapes base",
            )),
        });
    }
    let bytes = common::read_workspace_file(&base, &path).map_err(|e| PendingLoadError::Parse {
        path: Some(path.to_path_buf()),
        source: serde_json::Error::io(e),
    })?;
    parse_pending_bytes(&bytes, Some(path.to_path_buf()))
}

// ── Cross-bucket FK dependency graph ─────────────────────────────────────

/// Map every projected table name to its owning bucket, per database.
/// Input is the same `models` map compose already diffs against.
fn table_to_bucket_index(
    models: &std::collections::BTreeMap<BucketKey, AppliedSchema>,
) -> std::collections::BTreeMap<(String, String), BucketKey> {
    let mut idx = std::collections::BTreeMap::new();
    for (bucket, schema) in models {
        for table_name in schema.models.keys() {
            idx.insert(
                (bucket.database.clone(), table_name.clone()),
                bucket.clone(),
            );
        }
    }
    idx
}

/// Collect the FK target-table names an operation introduces.
/// Family-complete over every op variant in the shipped IR that
/// carries an FK reference: AddTable (inline column FKs), AddForeignKey,
/// AddColumn with an FK-bearing column.
///
/// Enumeration justification (REQ-398-2): every SchemaOperation variant
/// is accounted for below. The variants that introduce FK targets are
/// AddTable, AddColumn, and AddForeignKey. All others either remove
/// references, rename without adding new ones, or operate on non-FK
/// schema elements (indexes, enums, comments, PK flips, metadata).
fn fk_target_tables(op: &SchemaOperation) -> Vec<String> {
    let mut out = Vec::new();
    match op {
        // Included: inline column FKs in the new table definition
        SchemaOperation::AddTable(t) => {
            for col in &t.columns {
                if let Some(fk) = &col.foreign_key {
                    out.push(fk.ref_table.clone());
                }
            }
            // Period FK constraints are NOT part of TableSchema or the
            // diff IR today (schema-level struct + test-only emitters
            // only). If they ever join the pipeline, their ref_table
            // joins this extraction.
        }
        // Included: column-level foreign_key may reference another table
        SchemaOperation::AddColumn { column, .. } => {
            if let Some(fk) = &column.foreign_key {
                out.push(fk.ref_table.clone());
            }
        }
        // Included: explicit FK addition on an existing column
        SchemaOperation::AddForeignKey { fk, .. } => {
            out.push(fk.ref_table.clone());
        }
        // Excluded — no new FK reference introduced:
        // DropTable — removes a table, not a reference target
        // RenameTable — renames without adding references
        // DropColumn — removes a column
        // RenameColumn — renames without adding references
        // AlterColumn { .. } — changes type/nullability/default; does not add FK
        //   (FK changes go through AddForeignKey / DropForeignKey)
        // DropForeignKey { .. } — removes a reference, doesn't create one
        // AddIndex / DropIndex — index metadata only
        // AddExclusionConstraint / DropExclusionConstraint — constraint metadata
        // AddEnum / DropEnum / AddEnumVariant — enum type operations
        // PkTypeFlip / PkTypeFlipGroup / PkTypeFlipMultiGroup — PK migration
        // RenameApp — metadata-only rename
        // MoveModelBetweenApps — moves model ownership, no FK change
        // SetTableComment / SetStorageParams / SetTablespace — table metadata
        // Unsupported — differ refusal, not an operation
        SchemaOperation::DropTable { .. } => {}
        SchemaOperation::RenameTable { .. } => {}
        SchemaOperation::DropColumn { .. } => {}
        SchemaOperation::RenameColumn { .. } => {}
        SchemaOperation::AlterColumn { .. } => {}
        SchemaOperation::DropForeignKey { .. } => {}
        SchemaOperation::AddIndex(_) => {}
        SchemaOperation::DropIndex(_) => {}
        SchemaOperation::AddExclusionConstraint { .. } => {}
        SchemaOperation::DropExclusionConstraint { .. } => {}
        SchemaOperation::AddEnum(_) => {}
        SchemaOperation::DropEnum(_) => {}
        SchemaOperation::AddEnumVariant { .. } => {}
        SchemaOperation::PkTypeFlip { .. } => {}
        SchemaOperation::PkTypeFlipGroup(_) => {}
        SchemaOperation::PkTypeFlipMultiGroup(_) => {}
        SchemaOperation::RenameApp { .. } => {}
        SchemaOperation::MoveModelBetweenApps { .. } => {}
        SchemaOperation::SetTableComment { .. } => {}
        SchemaOperation::SetStorageParams { .. } => {}
        SchemaOperation::SetTablespace { .. } => {}
        SchemaOperation::Unsupported { .. } => {}
    }
    out
}

/// Cross-bucket dependency map: for each delta bucket, the set of
/// SAME-DATABASE buckets owning tables its new FKs reference.
/// Within-bucket refs are segment.rs's job; targets absent from the
/// index were already validated by projection (or pre-exist on disk)
/// and need no compose-run ordering edge.
/// Cross-bucket enum reconciliation: dedup AddEnum to one owner,
/// suppress AddEnum when another bucket's snapshot already has it,
/// defer DropEnum until last referencing bucket, and wire enum
/// ownership edges into the depends_on graph.
///
/// Returns ownership edges (same shape as `cross_bucket_dependencies`):
/// `BTreeMap<BucketKey, BTreeSet<String>>` mapping non-owner buckets
/// to the set of owner app names they depend on for enum types.
fn reconcile_enum_ops_across_buckets(
    deltas: &mut [SchemaDelta],
    models: &std::collections::BTreeMap<BucketKey, AppliedSchema>,
    snapshots: &std::collections::BTreeMap<BucketKey, AppliedSchema>,
    fk_cross_deps: &std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>>,
) -> Result<std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>>, ComposeError>
{
    use std::collections::{BTreeMap as BM, BTreeSet as BS};

    /// Intermediate decision: which (delta_index, enum_name) to remove.
    #[derive(Clone, Debug)]
    struct RemoveOp {
        idx: usize,
        enum_name: String,
        op_kind: OpKind,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum OpKind {
        AddEnum,
        DropEnum,
        AddEnumVariant { variant: String },
    }

    // Phase 1: Read — collect all data needed for decisions.
    // Group deltas by database and gather per-database context.
    let mut db_context: BM<String, (Vec<usize>, DBContext)> = BM::new();

    for delta in deltas.iter() {
        db_context
            .entry(delta.bucket.database.clone())
            .or_insert_with(|| {
                (
                    Vec::new(),
                    DBContext {
                        projected_enums: BM::new(),
                        snapshot_enums: BM::new(),
                        add_enum_ops: BM::new(),
                        drop_enum_ops: BM::new(),
                        add_enum_variant_ops: BM::new(),
                    },
                )
            });
    }

    // Populate projected_enums and snapshot_enums from ALL entries
    // in models/snapshots, not just effective delta buckets. No-op
    // deltas are filtered out before reconciliation (compose step 5),
    // but their schema context is still needed for cross-bucket
    // decisions: snapshot suppression (REQ-396-4) requires knowing
    // which enums exist in any bucket's snapshot, and drop deferral
    // (REQ-396-5) requires knowing which enums are still projected
    // by any bucket — including those with no pending delta.
    for (bucket, schema) in models {
        if let Some((_, ctx)) = db_context.get_mut(&bucket.database) {
            for name in schema.enums.keys() {
                ctx.projected_enums
                    .entry(bucket.clone())
                    .or_default()
                    .insert(name.clone());
            }
        }
    }
    for (bucket, schema) in snapshots {
        if let Some((_, ctx)) = db_context.get_mut(&bucket.database) {
            for name in schema.enums.keys() {
                ctx.snapshot_enums
                    .entry(bucket.clone())
                    .or_default()
                    .insert(name.clone());
            }
        }
    }

    for (i, delta) in deltas.iter().enumerate() {
        let db = delta.bucket.database.clone();
        let (indices, ctx) = db_context.entry(db).or_insert_with(|| {
            (
                Vec::new(),
                DBContext {
                    projected_enums: BM::new(),
                    snapshot_enums: BM::new(),
                    add_enum_ops: BM::new(),
                    drop_enum_ops: BM::new(),
                    add_enum_variant_ops: BM::new(),
                },
            )
        });
        indices.push(i);
        for op in &delta.operations {
            match op {
                SchemaOperation::AddEnum(e) => {
                    ctx.add_enum_ops
                        .insert((delta.bucket.clone(), e.name.clone()), i);
                }
                SchemaOperation::DropEnum(name) => {
                    ctx.drop_enum_ops
                        .entry((delta.bucket.clone(), name.clone()))
                        .or_default()
                        .push(i);
                }
                SchemaOperation::AddEnumVariant {
                    enum_name, variant, ..
                } => {
                    ctx.add_enum_variant_ops
                        .entry((enum_name.clone(), variant.clone()))
                        .or_default()
                        .push((delta.bucket.clone(), i));
                }
                _ => {}
            }
        }
    }

    // Phase 2: Decide — compute which ops to remove and which edges to add.
    let mut removes: Vec<RemoveOp> = Vec::new();
    let mut enum_edges: BM<BucketKey, BS<String>> = BM::new();

    for (db_name, (_indices, ctx)) in &db_context {
        // Gather all unique enum names.
        let mut all_enum_names: BS<String> = BS::new();
        for (_, name) in ctx.add_enum_ops.keys() {
            all_enum_names.insert(name.clone());
        }
        for (_, name) in ctx.drop_enum_ops.keys() {
            all_enum_names.insert(name.clone());
        }

        // Compute topological order for owner selection within this
        // database. Owner is the first bucket in `order_buckets` output
        // (FK-only deps, before enum edges are merged), so enum edges
        // never point opposite to FK edges and avoid false-positive cycles.
        let all_projected_apps: BS<String> = ctx
            .add_enum_ops
            .keys()
            .chain(ctx.drop_enum_ops.keys())
            .map(|(bk, _)| bk.app.clone())
            .chain(
                ctx.add_enum_variant_ops
                    .values()
                    .flat_map(|v| v.iter())
                    .map(|(bk, _)| bk.app.clone()),
            )
            .collect();
        let db_deps: BM<BucketKey, BS<String>> = fk_cross_deps
            .iter()
            .filter(|(bk, _)| bk.database == *db_name)
            .map(|(bk, deps)| (bk.clone(), deps.clone()))
            .collect();
        let topo_order = if all_projected_apps.len() > 1 {
            order_buckets(db_name, &all_projected_apps, &db_deps)?
        } else {
            all_projected_apps.into_iter().collect()
        };

        // 1. AddEnum dedup (REQ-396-3, REQ-396-4).
        for enum_name in &all_enum_names {
            let mut adders: Vec<(BucketKey, usize)> = ctx
                .add_enum_ops
                .iter()
                .filter(|((_, name), _)| name == enum_name)
                .map(|((bucket, _), idx)| (bucket.clone(), *idx))
                .collect();

            if adders.is_empty() {
                continue;
            }

            // Sort by topological position so owner is the first bucket
            // in the FK-based order — enum edges follow FK direction.
            adders.sort_by_key(|(bk, _)| topo_order.iter().position(|a| a == &bk.app).unwrap_or(0));

            // Check snapshot ownership: if any adder has the enum in
            // another bucket's snapshot, the type was created by a prior
            // migration — suppress all AddEnum to avoid duplicate CREATE TYPE.
            let has_snapshot_owner = adders.iter().any(|(adder_bucket, _)| {
                ctx.snapshot_enums.iter().any(|(snap_bucket, snap_set)| {
                    snap_bucket != adder_bucket && snap_set.contains(enum_name.as_str())
                })
            });

            if has_snapshot_owner {
                // Remove AddEnum from ALL adders.
                for (_bucket, idx) in &adders {
                    removes.push(RemoveOp {
                        idx: *idx,
                        enum_name: enum_name.clone(),
                        op_kind: OpKind::AddEnum,
                    });
                }
            } else {
                let (owner_bucket, _owner_idx) = &adders[0];
                for (bucket, idx) in &adders[1..] {
                    removes.push(RemoveOp {
                        idx: *idx,
                        enum_name: enum_name.clone(),
                        op_kind: OpKind::AddEnum,
                    });
                    enum_edges
                        .entry(bucket.clone())
                        .or_default()
                        .insert(owner_bucket.app.clone());
                }
            }
        }

        // 2. DropEnum deferral (REQ-396-5).
        for enum_name in &all_enum_names {
            let mut droppers: Vec<(BucketKey, usize)> = ctx
                .drop_enum_ops
                .iter()
                .filter(|((_, name), _)| name == enum_name)
                .flat_map(|((bucket, _), idxs)| idxs.iter().map(|&idx| (bucket.clone(), idx)))
                .collect();

            if droppers.is_empty() {
                continue;
            }

            let any_projected = ctx
                .projected_enums
                .values()
                .any(|en| en.contains(enum_name.as_str()));

            if any_projected {
                // Suppress all drops.
                for (_bucket, idx) in &droppers {
                    removes.push(RemoveOp {
                        idx: *idx,
                        enum_name: enum_name.clone(),
                        op_kind: OpKind::DropEnum,
                    });
                }
            } else {
                // Keep drop only for the LAST bucket in topological order,
                // so dependent buckets drop their references before the type
                // is removed. The keeper runs last and drops after everyone else.
                droppers.sort_by_key(|(bk, _)| {
                    topo_order.iter().position(|a| a == &bk.app).unwrap_or(0)
                });
                for (_bucket, idx) in &droppers[..droppers.len() - 1] {
                    removes.push(RemoveOp {
                        idx: *idx,
                        enum_name: enum_name.clone(),
                        op_kind: OpKind::DropEnum,
                    });
                }
            }
        }

        // 3. AddEnumVariant dedup + ownership edge.
        //    Two buckets adding the same (enum, variant) each emit
        //    `ALTER TYPE <e> ADD VALUE '<v>'`, which has no IF NOT EXISTS
        //    (REQ-396-11) — the second apply fails with "duplicate enum label".
        //    Keep the op on exactly one owner bucket; remove from the rest;
        //    wire the dependency edge so the owner's ADD VALUE runs first.
        for ((enum_name, variant), contributors) in &ctx.add_enum_variant_ops {
            // Ordering anchor: AddEnum owner this run, or first projecting bucket
            // if the type was created by a prior migration.
            let owner_bucket: Option<BucketKey> = ctx
                .add_enum_ops
                .iter()
                .find(|((_, name), _)| name == enum_name)
                .map(|((bucket, _), _)| bucket.clone())
                .or_else(|| {
                    ctx.projected_enums
                        .iter()
                        .find(|(_, names)| names.contains(enum_name.as_str()))
                        .map(|(bucket, _)| bucket.clone())
                });

            // Op-retention owner: first contributor in topo order.
            let mut adders = contributors.clone();
            adders.sort_by_key(|(bk, _)| topo_order.iter().position(|a| a == &bk.app).unwrap_or(0));

            let variant_owner = adders.first().map(|(bk, _)| bk.clone());

            for (bucket, idx) in &adders {
                // Remove AddEnumVariant from every non-variant-owner so only
                // one ALTER TYPE ... ADD VALUE fires per (enum, variant).
                // Single-adder case: adders.len() == 1, sole entry IS the
                // variant_owner, no RemoveOp pushed — existing ordering preserved.
                if variant_owner.as_ref() != Some(bucket) {
                    removes.push(RemoveOp {
                        idx: *idx,
                        enum_name: enum_name.clone(),
                        op_kind: OpKind::AddEnumVariant {
                            variant: variant.clone(),
                        },
                    });
                }
                // Ordering edge: non-owners depend on the ordering anchor.
                if let Some(ref ob) = owner_bucket
                    && bucket != ob
                {
                    enum_edges
                        .entry(bucket.clone())
                        .or_default()
                        .insert(ob.app.clone());
                }
            }
        }
    }

    // Phase 3: Apply — remove ops from deltas.
    for rm in &removes {
        match &rm.op_kind {
            OpKind::AddEnum => {
                deltas[rm.idx].operations.retain(
                    |op| !matches!(op, SchemaOperation::AddEnum(e) if e.name == rm.enum_name),
                );
            }
            OpKind::DropEnum => {
                deltas[rm.idx]
                    .operations
                    .retain(|op| !matches!(op, SchemaOperation::DropEnum(n) if n == &rm.enum_name));
            }
            OpKind::AddEnumVariant { variant } => {
                deltas[rm.idx].operations.retain(|op| {
                    !matches!(
                        op,
                        SchemaOperation::AddEnumVariant { enum_name, variant: v, .. }
                            if enum_name == &rm.enum_name && v == variant
                    )
                });
            }
        }
    }

    Ok(enum_edges)
}

/// Per-database context collected during Phase 1 (read).
#[derive(Default)]
struct DBContext {
    projected_enums: std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>>,
    snapshot_enums: std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>>,
    add_enum_ops: std::collections::BTreeMap<(BucketKey, String), usize>,
    drop_enum_ops: std::collections::BTreeMap<(BucketKey, String), Vec<usize>>,
    /// Keyed by `(enum_name, variant)` → list of `(bucket, delta_index)` that
    /// each want to emit `ALTER TYPE <enum> ADD VALUE '<variant>'`. When more
    /// than one contributor exists, all but the topo-first are removed to
    /// avoid the Postgres "duplicate enum label" error (no `IF NOT EXISTS`).
    add_enum_variant_ops: std::collections::BTreeMap<(String, String), Vec<(BucketKey, usize)>>,
}

/// Cross-bucket FK dependency map: for each delta bucket, the set of
/// SAME-DATABASE buckets owning tables its new FKs reference.
/// Within-bucket refs are segment.rs's job; targets absent from the
/// index were already validated by projection (or pre-exist on disk)
/// and need no compose-run ordering edge.
fn cross_bucket_dependencies(
    deltas: &[SchemaDelta],
    index: &std::collections::BTreeMap<(String, String), BucketKey>,
) -> std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>> {
    let mut deps: std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for delta in deltas {
        let entry = deps.entry(delta.bucket.clone()).or_default();
        for op in &delta.operations {
            for target in fk_target_tables(op) {
                let key = (delta.bucket.database.clone(), target);
                if let Some(owner) = index.get(&key) {
                    // Within-bucket refs are handled by segment.rs
                    if *owner != delta.bucket {
                        entry.insert(owner.app.clone());
                    }
                }
            }
        }
    }
    deps
}

/// Topologically order the composed buckets of one database by their
/// cross-bucket dependencies. Alphabetical tiebreak (precedent:
/// segment.rs toposort). Returns Err on a dependency cycle.
fn order_buckets(
    database: &str,
    buckets: &std::collections::BTreeSet<String>,
    deps: &std::collections::BTreeMap<BucketKey, std::collections::BTreeSet<String>>,
) -> Result<Vec<String>, ComposeError> {
    let mut in_degree: std::collections::BTreeMap<&str, usize> =
        buckets.iter().map(|b| (b.as_str(), 0)).collect();
    // rev[dep] = list of buckets that depend on `dep`
    let mut rev: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();

    for app in buckets {
        let key = BucketKey {
            database: database.to_string(),
            app: app.clone(),
        };
        if let Some(targets) = deps.get(&key) {
            for dep in targets {
                // Edges to buckets without a delta this run are ignored:
                // their tables already exist (REQ-398-6 compose-side twin).
                if buckets.contains(dep) {
                    *in_degree.get_mut(app.as_str()).expect("seeded") += 1;
                    rev.entry(dep.as_str()).or_default().push(app.as_str());
                }
            }
        }
    }

    let mut ready: std::collections::BTreeSet<&str> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(b, _)| *b)
        .collect();
    let mut out = Vec::with_capacity(buckets.len());

    while let Some(&next) = ready.iter().next() {
        ready.remove(next);
        out.push(next.to_string());
        for &dependent in rev.get(next).map(Vec::as_slice).unwrap_or(&[]) {
            let d = in_degree.get_mut(dependent).expect("seeded");
            *d -= 1;
            if *d == 0 {
                ready.insert(dependent);
            }
        }
    }

    if out.len() != buckets.len() {
        let chain: Vec<String> = in_degree
            .iter()
            .filter(|(_, d)| **d > 0)
            .map(|(b, _)| (*b).to_string())
            .collect();
        return Err(ComposeError::CrossBucketForeignKeyCycle {
            database: database.to_string(),
            chain,
        });
    }

    Ok(out)
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Run compose against the supplied request.
/// **Atomic per bucket.** Each bucket's three writes (up SQL, down
/// SQL, pending JSON) succeed together or roll back together. Across
/// buckets the writes are sequential — a failure on bucket N leaves
/// buckets 0..N composed and N+1..end uncomposed. Operators rerun
/// compose to clear the partial state.
/// **Acquires no locks itself.** The `_guard` parameter is the
/// caller's witness that the workspace lock is held — see
/// [`WorkspaceGuard`].
/// **Determinism.** Two invocations with the same `models`,
/// `snapshots`, `apps`, `name`, `allow_destructive` AND the same
/// `now` produce byte-identical output. Production callers pass
/// `OffsetDateTime::now_utc()`; tests pin a fixed instant.
pub fn compose(req: ComposeRequest<'_>) -> Result<ComposeReport, ComposeError> {
    let workspace_root =
        common::canonicalize_base(req.workspace_root).map_err(|source| ComposeError::Io {
            path: req.workspace_root.to_path_buf(),
            source,
        })?;

    // 0. Bootstrap auto-emit — for any database referenced in the
    // inputs that doesn't already have a bootstrap migration on disk,
    // emit one. This runs BEFORE the tombstone / differ /
    // classification / write logic because bootstrap is independent
    // of the descriptor delta — it's framework bootstrap (HeeRanjID
    // schema + Postgres extensions) that every
    // subsequent migration depends on.
    // Idempotent — emits nothing when the marker file already
    // exists. Once emitted, bootstrap is a regular committed
    // migration that the runner / `db reset` replays in lexical
    // version order (the all-zero `V00000000000000` prefix sorts
    // before any operator-composed migration).
    // Crucially, bootstrap emission is NOT gated on "delta has
    // operations" — a workspace can validly compose bootstrap even
    // when no model changes need a regular migration. The downstream
    // `NothingToCompose` check below considers ONLY the regular
    // delta path; bootstrap emissions count as compose progress on
    // their own (the report carries them in `emitted_phase_zero`).
    let emitted_phase_zero = if req.skip_phase_zero_auto_emit {
        Vec::new()
    } else {
        super::bootstrap::ensure_phase_zero_emitted(
            &workspace_root,
            req.models,
            req.apps,
            req.now,
            req._guard,
        )
        .map_err(ComposeError::PhaseZeroAutoEmit)?
    };

    // 1. Collect tombstone violations BEFORE any work — fail loudly
    // when an active model OR a stale snapshot still references a
    // tombstoned app.
    // D011 fires whenever a tombstoned app still has schema state
    // to drop, regardless of whether that state lives in `models`
    // (developer hasn't yet removed the structs) or in the snapshot
    // (developer removed the structs but the schema is still applied
    // to the database). The previous guard `!s.models.is_empty`
    // skipped the snapshot-only path and let the destructive
    // classification fire generically — losing the D011 specificity.
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

    // 2. Rewrite snapshot bucket keys for renamed apps BEFORE running
    // the differ. The on-disk SQL tables don't move when an app
    // renames — only the `app_label` ledger column and the
    // `migrations/<db>/<app>/` folder do. The pre-rename snapshot
    // still describes the same physical tables; under the NEW app
    // label they are unchanged. By rewriting the OLD bucket's
    // snapshot key to NEW before diffing, the differ sees the
    // tables as already-present on both sides and emits no spurious
    // DropTable on OLD / AddTable on NEW. Without this rewrite a
    // rename would always require `--allow-destructive` even though
    // the operation is metadata-only.
    let snapshots_for_diff = remap_snapshots_for_renames(req.snapshots, req.apps);

    // 2b. REQ-370-16 — linkage-aware drop guard. Evaluated on the POST-REMAP
    // snapshots (snapshots_for_diff) — the exact view the differ is
    // about to diff — so renames (already relabeled to their NEW key
    // by remap_snapshots_for_renames) carry their models forward and
    // never trip the guard.
    // For every snapshot BUCKET that still describes schema state on
    // disk but for which the CURRENT projection (req.models) carries
    // ZERO models, refuse — UNLESS that bucket's app is tombstoned
    // (the intentional-removal channel). Keys on the bucket's
    // (database, app) and on "zero projected models", NOT on
    // snap.registered_apps (DB-global, shared across buckets — looping
    // it would false-positive). The synthetic global bucket is guarded
    // uniformly — un-#[model(app=)] models live there, and a bucket
    // that HAD models and now has zero is a real removal.
    // Fires for any snapshot bucket with tables whose projection
    // has zero models, regardless of whether the app descriptor
    // exists in req.apps. The tombstone check provides the
    // intentional-removal exemption; if no app exists at all,
    // there can't be a tombstone, so the guard fires correctly.
    // Fires even with --allow-destructive: the generic destructive
    // gate only covers the default path; this guard's job is the
    // --allow-destructive residual data-loss path.
    {
        use std::collections::BTreeSet;

        let tombstoned: BTreeSet<(&str, &str)> = req
            .apps
            .iter()
            .filter(|a| a.tombstone)
            .map(|a| (a.database.as_str(), a.label.as_str()))
            .collect();

        let app_has_models = |database: &str, app: &str| -> bool {
            let bucket = BucketKey {
                database: database.to_string(),
                app: app.to_string(),
            };
            req.models
                .get(&bucket)
                .is_some_and(|s| !s.models.is_empty())
        };

        for (bucket, snap) in snapshots_for_diff.iter() {
            if snap.models.is_empty() {
                continue; // no tables to drop
            }
            let database = bucket.database.as_str();
            let app = bucket.app.as_str();
            if tombstoned.contains(&(database, app)) {
                continue; // intentional removal channel
            }
            if app_has_models(database, app) {
                continue; // app still linked + projects models
            }
            let display_label = super::target::app_dirname(app);
            let text = format!(
                "app \"{display_label}\" was previously registered (database \"{database}\") \
                 but no models for it are linked now — did you forget to link its crate? \
                 Refusing to emit DROPs. If this removal is intentional, mark the app \
                 `#[app(tombstone)]`."
            );
            return Err(ComposeError::LinkageDropWithoutModels {
                app_label: app.to_string(),
                database: database.to_string(),
                text,
            });
        }
    }

    // 3. Run the differ across the (possibly remapped) bucket map.
    // The differ now returns Result; cascade-depth blow-outs surface
    // as `ComposeError::Diff` rather than panicking.
    let mut deltas =
        diff_bucket_maps(&snapshots_for_diff, req.models).map_err(ComposeError::Diff)?;

    // 3b. Apply operator-configured join-table cutover layout to every
    // PK-flip group the differ emitted. Without this step the
    // `MigrateConfig::pk_flip_join_table_option` knob would have
    // no effect — the differ defaults every group to Option A and
    // only this hook overrides it.
    if let Some(option) = req.pk_flip_join_table_option {
        super::diff::apply_pk_flip_join_table_option(&mut deltas, option);
    }

    // 4. Layer in `RenameApp` ops driven by `AppRegistry`'s
    // `renamed_from` field. The differ doesn't see this — it works
    // purely on snapshots — so compose injects the op on the
    // DESTINATION bucket (the post-rename label).
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
    // `NoOp` and an empty operations vec; skip them. Renamed-only
    // deltas DO carry operations and survive the filter.
    let mut effective: Vec<SchemaDelta> = deltas
        .into_iter()
        .filter(|d| !d.operations.is_empty() || !matches!(d.classification, Classification::NoOp))
        .collect();

    if effective.is_empty() {
        // When the regular delta path has nothing to do BUT bootstrap
        // was emitted this run, the compose is NOT a no-op — bootstrap
        // is real progress that the operator will apply via
        // `migrations apply`. Surface a successful report so the CLI's
        // friendly "composed N bootstrap migrations" line prints,
        // instead of the `NothingToCompose` exit-zero quiet path.
        // The reverse case — bootstrap already on disk AND no delta
        // changes — surfaces `NothingToCompose` as before. That keeps
        // the "all in sync" message intact.
        if !emitted_phase_zero.is_empty() {
            return Ok(ComposeReport {
                composed_buckets: Vec::new(),
                emitted_phase_zero,
                converged_snapshot_buckets: Vec::new(),
            });
        }
        return Err(ComposeError::NothingToCompose);
    }

    // 5b. Hoist cross-bucket FK dependency graph so enum ownership
    // edges merge before cycle detection (REQ-396-3).
    let bucket_index = table_to_bucket_index(req.models);
    let mut cross_deps = cross_bucket_dependencies(&effective, &bucket_index);

    // Capture pre-reconciliation bucket set BEFORE 5c modifies effective in-place.
    // Buckets that had ops before reconciliation but are absent or empty after are
    // the convergence candidates — they need a silent snapshot write.
    let buckets_with_ops_pre_reconcile: std::collections::BTreeSet<BucketKey> = effective
        .iter()
        .filter(|d| !d.operations.is_empty())
        .map(|d| d.bucket.clone())
        .collect();

    // 5c. Cross-bucket enum reconciliation — dedup AddEnum to one
    // owner, defer DropEnum until last referencing bucket, and
    // suppress enums already recorded by another bucket's snapshot.
    let enum_edges =
        reconcile_enum_ops_across_buckets(&mut effective, req.models, req.snapshots, &cross_deps)?;

    // 5d. Merge enum ownership edges into cross_deps before cycle check.
    for (bucket, deps) in &enum_edges {
        cross_deps
            .entry(bucket.clone())
            .or_default()
            .extend(deps.iter().cloned());
    }

    // 5e. Re-classify deltas whose ops were modified by reconciliation.
    for delta in &mut effective {
        delta.classification = super::diff::classify_operations(&delta.operations);
    }

    // 5f. Drop deltas that became empty after reconciliation.
    effective
        .retain(|d| !d.operations.is_empty() || !matches!(d.classification, Classification::NoOp));

    // Identify buckets reconciliation emptied: had ops before reconciliation,
    // absent from effective (or empty) after retain. These buckets need a
    // silent snapshot convergence write so build.rs sees no drift.
    let converged_snapshot_buckets: Vec<BucketKey> = {
        let effective_buckets_post_retain: std::collections::BTreeSet<&BucketKey> =
            effective.iter().map(|d| &d.bucket).collect();
        buckets_with_ops_pre_reconcile
            .into_iter()
            .filter(|bk| !effective_buckets_post_retain.contains(bk))
            .collect()
    }; // effective_buckets_post_retain borrow dropped here

    // Write the current scoped model snapshot for each converged bucket so
    // build.rs sees no drift on the next run. No SQL, no version file — only
    // the snapshot JSON. This is idempotent: the same scoped snapshot would
    // be written again if compose runs again before apply (no harm done).
    for bucket in &converged_snapshot_buckets {
        let snap_path = snapshot_path(&workspace_root, bucket);
        let current_snap = req
            .models
            .get(bucket)
            .cloned()
            .unwrap_or_else(|| empty_schema_for(bucket));
        let snap_bytes = serialize_snapshot(&current_snap).map_err(|e| ComposeError::Io {
            path: snap_path.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        ensure_parent(&snap_path)?;
        let snap_tmp = atomic_write(&snap_path, &snap_bytes)?;
        let snap_backup = promote_tmp_with_backup(&snap_tmp, &snap_path)?;
        // We intentionally do NOT enrol these in the rollback guard: a
        // convergence snapshot is an advance of the "last seen" marker —
        // rolling it back on a compose failure would leave the workspace in
        // the same stale-snapshot state we're trying to fix. The write is
        // safe to leave in place even if the rest of compose fails (no SQL
        // was emitted, so apply won't advance past this point).
        // Cleanup the backup ourselves since we won't call rollback.commit().
        if let Some(bak) = snap_backup {
            let _ = fs::remove_file(bak);
        }
    }

    // After reconciliation, check if all deltas were consumed.
    if effective.is_empty() {
        if !emitted_phase_zero.is_empty() {
            return Ok(ComposeReport {
                composed_buckets: Vec::new(),
                emitted_phase_zero,
                converged_snapshot_buckets,
            });
        }
        if !converged_snapshot_buckets.is_empty() {
            // Convergence snapshots were written; no new migrations — this is
            // a successful silent-sync rather than a true NothingToCompose.
            // Surface a ComposeReport with empty composed_buckets so callers
            // can log "snapshot converged for N buckets" without treating it
            // as an error.
            return Ok(ComposeReport {
                composed_buckets: Vec::new(),
                emitted_phase_zero,
                converged_snapshot_buckets,
            });
        }
        return Err(ComposeError::NothingToCompose);
    }

    // 6. Classification gates — use fresh classifications from step 5e.
    for delta in &mut effective {
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

    // 6b. Cycle detection and ordering using merged cross_deps.

    // Group buckets by database for per-database topological sort.
    let mut db_buckets: std::collections::BTreeMap<&str, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for delta in &effective {
        db_buckets
            .entry(delta.bucket.database.as_str())
            .or_default()
            .insert(delta.bucket.app.clone());
    }

    // Fail before writing any artifact if a cycle is detected.
    // The returned order is intentionally discarded — compose writes per-bucket
    // artifacts without ordering; the apply phase re-derives execution order
    // from depends_on fields in the written pending plans.
    for (database, buckets) in &db_buckets {
        order_buckets(database, buckets, &cross_deps)?;
    }

    // Build depends_on map: bucket_key -> list of dependency app names.
    // FK cross-bucket edges are filtered to same-cycle targets (the
    // referenced table must have a delta this run), but enum ownership
    // edges are preserved unconditionally — the owner may be a prior-cycle
    // migration with no current delta, yet the variant bucket still needs
    // the ordering guarantee that the enum type was created first.
    let effective_apps: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = {
        let mut map: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for delta in &effective {
            map.entry(delta.bucket.database.clone())
                .or_default()
                .insert(delta.bucket.app.clone());
        }
        map
    };

    let mut depends_on_map: std::collections::BTreeMap<BucketKey, Vec<String>> = cross_deps
        .iter()
        .map(|(key, targets)| {
            (
                key.clone(),
                effective_apps
                    .get(key.database.as_str())
                    .map(|apps| targets.intersection(apps).cloned().collect())
                    .unwrap_or_default(),
            )
        })
        .collect();

    // Restore enum ownership edges that were filtered out by effective_apps.
    // FK cross-deps need same-cycle targets, but enum edges may reference
    // prior-cycle owners with no current delta. Use BTreeSet to avoid
    // duplicates when the owner IS in effective (already present via
    // cross_deps -> depends_on_map).
    for (bucket, deps) in &enum_edges {
        let mut target_set: std::collections::BTreeSet<String> = depends_on_map
            .get(bucket)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        target_set.extend(deps.iter().cloned());
        depends_on_map.insert(bucket.clone(), target_set.into_iter().collect());
    }

    // 7. Lower each delta to SQL pairs + plan, write all artifacts.
    // The write dance per bucket:
    // - Compute the lowered SQL pair + checksums.
    // - Inject the ledger UPDATE leg for any RenameApp ops.
    // mandates that the rename-exception ledger UPDATE ride along
    // with the migration's up/down.
    // - D013 check: refuse to overwrite a hand-edited file unless
    // `force_overwrite` is set.
    // - Stage four sibling tmp files (up SQL, down SQL, replay plan,
    // pending JSON), tracked under a `WriteRollback` Drop guard so
    // any mid-sequence failure removes ALL staged tmps + already-
    // promoted finals.
    // - Promote each tmp to its final path; on success commit the
    // guard.
    // - Per RenameApp delta, atomically rename the OLD bucket
    // directory to the NEW bucket directory after artifacts land.
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
            let mut replay_plan = plan_delta(delta).map_err(ComposeError::SqlEmit)?;
            let mut executable_ops: Vec<OperationSql> = replay_plan
                .segments
                .iter()
                .flat_map(|segment| segment.statements.iter().cloned())
                .collect();

            // For each RenameApp op, append an OperationSql that
            // updates `djogi_schema_migrations.app_label` so the ledger
            // is consistent with the new bucket name. The metadata-only
            // OperationSql produced by the standard emitter carries only
            // comments; we layer the real DDL here so it's hashed into
            // `checksum_up` and reviewable in the on-disk SQL file.
            let mut folder_renames_for_delta: Vec<(String, String)> = Vec::new();
            let mut replay_tail_sql: Vec<OperationSql> = Vec::new();
            for op in &delta.operations {
                if let SchemaOperation::RenameApp { from, to } = op {
                    let rename_stmt =
                        emit_rename_app_ledger_update(&delta.bucket.database, from, to);
                    lowered.push(rename_stmt.clone());
                    executable_ops.push(rename_stmt.clone());
                    replay_tail_sql.push(rename_stmt);
                    folder_renames_for_delta.push((from.clone(), to.clone()));
                }
            }
            if !replay_tail_sql.is_empty() {
                replay_plan.segments.push(Segment {
                    kind: SegmentKind::Transactional,
                    statements: replay_tail_sql,
                });
            }

            let model_snapshot = req
                .models
                .get(&delta.bucket)
                .cloned()
                .unwrap_or_else(|| empty_schema_for(&delta.bucket));

            let (checksum_up, checksum_down) = compute_checksums(&executable_ops);

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
                depends_on: depends_on_map
                    .get(&delta.bucket)
                    .cloned()
                    .unwrap_or_default(),
            };

            let bucket_path = bucket_dir(&workspace_root, &delta.bucket);
            let up_candidate = bucket_path.join(up_filename(&version));
            let down_candidate = bucket_path.join(down_filename(&version));
            let replay_plan_candidate =
                committed_replay_plan_path(&workspace_root, &delta.bucket, &version);
            let pending_candidate = pending_json_path(&workspace_root, &delta.bucket);

            let up_path = common::resolve_write_workspace_path(&workspace_root, &up_candidate)
                .map_err(|e| ComposeError::Io {
                    path: up_candidate.clone(),
                    source: e,
                })?;
            let down_path = common::resolve_write_workspace_path(&workspace_root, &down_candidate)
                .map_err(|e| ComposeError::Io {
                    path: down_candidate.clone(),
                    source: e,
                })?;
            let replay_plan_path =
                common::resolve_write_workspace_path(&workspace_root, &replay_plan_candidate)
                    .map_err(|e| ComposeError::Io {
                        path: replay_plan_candidate.clone(),
                        source: e,
                    })?;
            let pending_path =
                common::resolve_write_workspace_path(&workspace_root, &pending_candidate).map_err(
                    |e| ComposeError::Io {
                        path: pending_candidate.clone(),
                        source: e,
                    },
                )?;

            let up_sql = compose_up_text(&version, delta, &lowered);
            let down_sql = compose_down_text(&version, delta, &lowered);
            let replay_plan_bytes = serialize_committed_replay_plan(
                &replay_plan,
                &checksum_up,
                checksum_down.as_deref(),
            )
            .map_err(|e| {
                ComposeError::SerializeFailed(SnapshotError::Parse {
                    source: e,
                    path: None,
                })
            })?;
            let pending_bytes = serialize_pending(&pending)?;

            // D013 hand-edit protection.
            // Protect BOTH up AND down SQL. If either file already
            // exists and its current bytes differ from what compose
            // would emit fresh, the operator has hand edited the
            // migration. Without `force_overwrite` we refuse to clobber.
            // The comparison uses full byte equality (not a separate
            // checksum) because the emitter is deterministic — same
            // inputs always produce the same bytes — so byte-equality
            // is exactly equivalent to a checksum match without
            // re-derivation.
            if !req.force_overwrite {
                check_no_hand_edit(
                    &workspace_root,
                    &up_path,
                    up_sql.as_bytes(),
                    &down_path,
                    down_sql.as_bytes(),
                    &delta.bucket,
                )?;
            }
            check_pending_path_compatible(&workspace_root, &pending_path, &delta.bucket)?;

            // Stage tmp siblings.
            ensure_parent(&up_path)?;
            ensure_parent(&pending_path)?;
            let up_tmp = atomic_write(&up_path, up_sql.as_bytes())?;
            rollback.track_tmp(up_tmp.clone());

            let down_tmp = atomic_write(&down_path, down_sql.as_bytes())?;
            rollback.track_tmp(down_tmp.clone());

            let replay_plan_tmp = atomic_write(&replay_plan_path, &replay_plan_bytes)?;
            rollback.track_tmp(replay_plan_tmp.clone());

            let pending_tmp = atomic_write(&pending_path, &pending_bytes)?;
            rollback.track_tmp(pending_tmp.clone());

            // Promote tmps. Order: up SQL, down SQL, replay sidecar, pending JSON.
            // Each promote captures any prior bytes into a sibling
            // backup file BEFORE renaming the tmp into place; the
            // `WriteRollback` guard records the backup alongside the
            // final path so a later failure restores the original
            // content. On commit (success path) the backups are deleted.
            let up_backup = promote_tmp_with_backup(&up_tmp, &up_path)?;
            rollback.promote(&up_tmp, up_path.clone(), up_backup);

            let down_backup = promote_tmp_with_backup(&down_tmp, &down_path)?;
            rollback.promote(&down_tmp, down_path.clone(), down_backup);

            let replay_plan_backup = promote_tmp_with_backup(&replay_plan_tmp, &replay_plan_path)?;
            rollback.promote(
                &replay_plan_tmp,
                replay_plan_path.clone(),
                replay_plan_backup,
            );

            let pending_backup = promote_tmp_with_backup(&pending_tmp, &pending_path)?;
            rollback.promote(&pending_tmp, pending_path.clone(), pending_backup);

            // Queue any RenameApp folder moves. We perform them after
            // every artifact write succeeds because a folder rename is
            // hard to roll back atomically in conjunction with the file
            // writes — the conservative posture is to write first,
            // rename second.
            for (from_label, _to_label) in folder_renames_for_delta {
                let from_bucket = BucketKey {
                    database: delta.bucket.database.clone(),
                    app: from_label,
                };
                let from_dir = bucket_dir(&workspace_root, &from_bucket);
                let to_dir = bucket_dir(&workspace_root, &delta.bucket);
                pending_folder_renames.push((from_dir, to_dir));
            }

            composed_buckets.push(ComposedBucket {
                bucket: delta.bucket.clone(),
                version: version.clone(),
                up_sql_path: up_path,
                down_sql_path: down_path,
                replay_plan_path,
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
    // RenameApp ops. The merge step tracks every entry move on the
    // same `rollback` guard so a mid-loop failure rolls back every
    // already-moved entry too.
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
        converged_snapshot_buckets,
    })
}

/// D013 — refuse to overwrite a hand-edited migration.
/// Compares the existing up AND down SQL files' bytes to what compose
/// would emit fresh. When EITHER side's existing bytes differ from
/// the freshly-emitted bytes the operator has hand edited the
/// migration; we surface
/// [`ComposeError::HandEditedMigrationWouldBeOverwritten`] (D013)
/// rather than silently clobber. The down side was previously
/// unprotected — a hand-edit there would have been silently
/// overwritten.
/// We compare full bytes rather than a separate checksum because
/// `compose_up_text` / `compose_down_text` are deterministic — same
/// inputs always produce the same bytes — so byte-equality is
/// exactly equivalent to "checksum matches" without needing a
/// reverse-engineering pass over the formatted SQL file. This is the
/// canonical D013 check; the doc comment on
/// `ComposeError::HandEditedMigrationWouldBeOverwritten` describes
/// the byte-equality semantics directly.)
/// The reported `path` and `side` describe which side was edited:
/// - Up only edited → `path = up_path`, side label "up".
/// - Down only edited → `path = down_path`, side label "down".
/// - Both edited → `path = up_path`, side label "up and down" (the up
///   path is reported because the operator typically inspects the up
///   file first).
///   Returns `Ok(())` when:
/// - Both files do not exist (first compose for this bucket).
/// - The existing files' bytes both match the freshly-emitted bytes.
fn check_no_hand_edit(
    workspace_root: &Path,
    up_path: &Path,
    fresh_up_bytes: &[u8],
    down_path: &Path,
    fresh_down_bytes: &[u8],
    bucket: &BucketKey,
) -> Result<(), ComposeError> {
    let up_edited = match common::read_workspace_file(workspace_root, up_path) {
        Ok(existing) => existing != fresh_up_bytes,
        Err(_) => false, // file missing — fresh compose, no clobber risk.
    };
    let down_edited = match common::read_workspace_file(workspace_root, down_path) {
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

/// Emit the ledger UPDATE leg for a RenameApp delta.
/// Per ("rename exception to append-only ledger"), the ledger
/// row's `app_label` for every prior migration must be updated when an
/// app is renamed. We append this as a real `OperationSql` to the
/// lowered list so it gets:
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

/// Atomically rename the OLD bucket directory to the NEW bucket
/// directory.
/// Called after every artifact write succeeds so the workspace is
/// consistent on disk. Skips silently when:
/// - The OLD directory does not exist (nothing to rename).
/// - The OLD and NEW directories are identical (a same-app
///   "self-rename" is a no-op — should not happen but defensive).
///   When the NEW directory already exists (the typical case — compose
///   just wrote artifacts there), we MOVE every entry from OLD to NEW.
///   Each entry move is tracked through the supplied [`WriteRollback`]
///   guard so a mid-loop failure rolls back every already-moved entry.
///   We ALSO refuse fail-fast on a content collision: if any entry under
///   OLD already exists under NEW with a different name-equivalent
///   location, we return [`ComposeError::FolderRenameTargetCollision`]
///   before moving any entry — the prior shape silently skipped
///   collisions and dropped the OLD entry, which conflated two distinct
///   files of the same name. The operator must resolve the collision
///   manually before re-running compose.
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

    // Pre-flight collision check. We refuse to silently overwrite
    // any newly-composed artifact in NEW.
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

/// Relabel any OLD-bucket snapshot under its renamed-to label BEFORE
/// the differ runs.
/// Why: an `#[app(renamed_from = "old")]` annotation tells compose
/// that the app's logical label changed but its physical schema did
/// not. The pre-rename snapshot was keyed under `BucketKey { app:
/// "old", .. }`; the new model inventory keys the same tables under
/// `BucketKey { app: "new", .. }`. If the differ sees both keys it
/// emits `DropTable` on OLD and `AddTable` on NEW for every model in
/// the bucket — escalating the rename to a destructive classification
/// that wrongly demands `--allow-destructive` and re-creates every
/// table from scratch.
/// The fix: walk `apps` for renamed-from entries and rebuild
/// `snapshots` so the OLD bucket's snapshot value lives under the NEW
/// bucket's key. The differ then sees a single bucket on both sides
/// (NEW) with identical models — no drops, no adds, just possibly
/// column-level diffs the operator legitimately introduced.
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
    out.push_str("--\n");
    out.push_str("-- Apply via `djogi migrations apply`, not psql. This file is a review\n");
    out.push_str("-- artifact and replay source. Manual execution bypasses ledger recording,\n");
    out.push_str("-- checksum verification, advisory locking, and snapshot advancement.\n");
    out.push_str("-- Direct execution is only appropriate for debugging, audit transparency,\n");
    out.push_str("-- or explicit operator override.\n");
    out.push_str("--\n");
    out.push_str("-- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n");
    if requires_numeric_array_helper(lowered) {
        out.push_str(NUMERIC_ARRAY_HELPER_PRELUDE);
        out.push('\n');
    }
    if requires_date_array_helper(lowered) {
        out.push_str(DATE_ARRAY_HELPER_PRELUDE);
        out.push('\n');
    }
    if requires_tstz_array_helper(lowered) {
        out.push_str(TSTZ_ARRAY_HELPER_PRELUDE);
        out.push('\n');
    }
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
    out.push_str("--\n");
    out.push_str("-- Apply via `djogi migrations apply`, not psql. This file is a review\n");
    out.push_str("-- artifact and replay source. Manual execution bypasses ledger recording,\n");
    out.push_str("-- checksum verification, advisory locking, and snapshot advancement.\n");
    out.push_str("-- Direct execution is only appropriate for debugging, audit transparency,\n");
    out.push_str("-- or explicit operator override.\n");
    out.push_str("--\n");
    out.push_str("-- DO NOT EDIT — regenerate via `djogi migrations compose`.\n\n");
    if requires_numeric_array_helper(lowered) {
        out.push_str(NUMERIC_ARRAY_HELPER_PRELUDE);
        out.push('\n');
    }
    if requires_date_array_helper(lowered) {
        out.push_str(DATE_ARRAY_HELPER_PRELUDE);
        out.push('\n');
    }
    if requires_tstz_array_helper(lowered) {
        out.push_str(TSTZ_ARRAY_HELPER_PRELUDE);
        out.push('\n');
    }
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

/// Name fragment used by both the numeric-array CHECK projection and the
/// helper function body.
const NUMERIC_ARRAY_HELPER_MARKER: &str = "djogi.__djogi_numeric_array_is_rust_decimal_v1(";

/// Canonical helper prelude for `FieldSqlType::NumericArray` checks.
/// Kept `pub(crate)` so segment planning can reuse the exact same body
/// when injecting helper DDL into executable plans.
/// The body mirrors the scalar `decimal_repr_expr` projection in
/// `migrate::projection`: each non-NULL element must be a finite
/// NUMERIC representable by `rust_decimal::Decimal`. The leading
/// `pg_catalog.scale(value) IS NOT NULL` clause rejects the three
/// PostgreSQL NUMERIC special values (`NaN`, `Infinity`, `-Infinity`)
/// that `pg_catalog.scale()` is defined to map to NULL — without that
/// guard the later `scale <= 28` / coefficient clauses would
/// NULL-propagate and `bool_and` would treat the special-value
/// element as satisfied, silently admitting an array element that
/// would later fail `Decimal::from_sql` on read with
/// `DjogiError::Decode`. The `value IS NULL OR (...)` outer guard
/// continues to admit `NULL` elements per array semantics.
pub(crate) const NUMERIC_ARRAY_HELPER_PRELUDE: &str = r#"CREATE SCHEMA IF NOT EXISTS djogi;

CREATE OR REPLACE FUNCTION djogi.__djogi_numeric_array_is_rust_decimal_v1(input_array pg_catalog.numeric[])
RETURNS pg_catalog.bool
LANGUAGE sql
IMMUTABLE STRICT PARALLEL SAFE
AS $$
    SELECT COALESCE(
        pg_catalog.bool_and(
            value IS NULL
            OR (
                pg_catalog.scale(value) IS NOT NULL
                AND pg_catalog.scale(value) <= 28
                AND pg_catalog.abs(value)
                    * pg_catalog.power(10::pg_catalog.numeric, pg_catalog.scale(value))
                    <= 79228162514264337593543950335::pg_catalog.numeric
            )
        ),
        true
    )
    FROM pg_catalog.unnest(input_array) AS value(value);
$$;
"#;

pub(crate) fn requires_numeric_array_helper(operations: &[OperationSql]) -> bool {
    operations.iter().any(|op| {
        op.up.contains(NUMERIC_ARRAY_HELPER_MARKER) || op.down.contains(NUMERIC_ARRAY_HELPER_MARKER)
    })
}

pub(crate) fn numeric_array_helper_operation() -> OperationSql {
    OperationSql {
        label: "Ensure djogi numeric-array helper".to_string(),
        up: NUMERIC_ARRAY_HELPER_PRELUDE.to_string(),
        down: "-- no-op rollback placeholder: helper is shared by framework CHECK constraints"
            .to_string(),
        lossy: None,
    }
}

/// Name fragment used by both the `FieldSqlType::DateArray` CHECK projection and the
/// helper function body.
/// The helper is the only CHECK-valid way to apply `pg_catalog.isfinite` per element
/// in a `date[]` column: Postgres CHECK clauses may not contain subqueries or `unnest`
/// aggregate forms directly.
const DATE_ARRAY_HELPER_MARKER: &str = "djogi.__djogi_date_array_is_finite_v1(";

/// Name fragment used by both the `FieldSqlType::TimestamptzArray` CHECK projection and
/// the helper function body.
const TSTZ_ARRAY_HELPER_MARKER: &str = "djogi.__djogi_tstz_array_is_finite_v1(";

/// Canonical helper prelude for `FieldSqlType::DateArray` checks.
/// Kept `pub(crate)` so segment planning can reuse the exact same body when injecting
/// helper DDL into executable plans.
/// The function mirrors the scalar `date_range_expr` predicate in
/// `migrate::projection`: each non-NULL element must be finite (both `+infinity` and
/// `-infinity` are rejected by `pg_catalog.isfinite`) AND not exceed `time::Date`'s
/// representable maximum (`9999-12-31`). The leading `pg_catalog.isfinite(value)` guard
/// is the key addition over the old `upper_bound >= ALL(col)` strategy — without it
/// `-infinity::date` passes because `upper_bound >= -infinity` is TRUE in Postgres
/// ordering, silently landing an element that would poison the next typed
/// `time::Date::from_sql` decode with `DjogiError::Decode`.
/// The `value IS NULL OR (...)` inner guard admits NULL elements per array semantics.
/// `COALESCE(..., true)` maps the empty-set `pg_catalog.bool_and` NULL to TRUE so
/// empty arrays pass the CHECK.
pub(crate) const DATE_ARRAY_HELPER_PRELUDE: &str = r#"CREATE SCHEMA IF NOT EXISTS djogi;

CREATE OR REPLACE FUNCTION djogi.__djogi_date_array_is_finite_v1(input_array pg_catalog.date[])
RETURNS pg_catalog.bool
LANGUAGE sql
IMMUTABLE STRICT PARALLEL SAFE
AS $$
    SELECT COALESCE(
        pg_catalog.bool_and(
            value IS NULL
            OR (
                pg_catalog.isfinite(value)
                AND value <= '9999-12-31'::pg_catalog.date
            )
        ),
        true
    )
    FROM pg_catalog.unnest(input_array) AS value(value);
$$;
"#;

/// Canonical helper prelude for `FieldSqlType::TimestamptzArray` checks.
/// Kept `pub(crate)` so segment planning can reuse the exact same body when injecting
/// helper DDL into executable plans.
/// Same shape as [`DATE_ARRAY_HELPER_PRELUDE`] for `timestamptz` elements. The inner
/// `pg_catalog.isfinite(value)` clause rejects both non-finite `timestamptz` special
/// values (`+infinity`, `-infinity`). The upper-bound literal uses the explicit `+00`
/// UTC offset so the comparison is timezone-invariant — using plain `TIMESTAMP '...'`
/// (without TZ) would make Postgres interpret the literal in the session timezone,
/// shifting the effective upper bound.
pub(crate) const TSTZ_ARRAY_HELPER_PRELUDE: &str = r#"CREATE SCHEMA IF NOT EXISTS djogi;

CREATE OR REPLACE FUNCTION djogi.__djogi_tstz_array_is_finite_v1(input_array pg_catalog.timestamptz[])
RETURNS pg_catalog.bool
LANGUAGE sql
IMMUTABLE STRICT PARALLEL SAFE
AS $$
    SELECT COALESCE(
        pg_catalog.bool_and(
            value IS NULL
            OR (
                pg_catalog.isfinite(value)
                AND value <= '9999-12-31 23:59:59.999999+00'::pg_catalog.timestamptz
            )
        ),
        true
    )
    FROM pg_catalog.unnest(input_array) AS value(value);
$$;
"#;

/// Returns `true` if any operation in `operations` references the date-array finite
/// helper — signalling that [`DATE_ARRAY_HELPER_PRELUDE`] must be prepended.
pub(crate) fn requires_date_array_helper(operations: &[OperationSql]) -> bool {
    operations.iter().any(|op| {
        op.up.contains(DATE_ARRAY_HELPER_MARKER) || op.down.contains(DATE_ARRAY_HELPER_MARKER)
    })
}

/// Returns `true` if any operation in `operations` references the tstz-array finite
/// helper — signalling that [`TSTZ_ARRAY_HELPER_PRELUDE`] must be prepended.
pub(crate) fn requires_tstz_array_helper(operations: &[OperationSql]) -> bool {
    operations.iter().any(|op| {
        op.up.contains(TSTZ_ARRAY_HELPER_MARKER) || op.down.contains(TSTZ_ARRAY_HELPER_MARKER)
    })
}

/// `OperationSql` wrapper for [`DATE_ARRAY_HELPER_PRELUDE`].
/// Segment planning inserts this at position 0 (before any column/table DDL) so the
/// function exists before the first CHECK that references it.
pub(crate) fn date_array_helper_operation() -> OperationSql {
    OperationSql {
        label: "Ensure djogi date-array finite-element helper".to_string(),
        up: DATE_ARRAY_HELPER_PRELUDE.to_string(),
        down: "-- no-op rollback placeholder: helper is shared by framework CHECK constraints"
            .to_string(),
        lossy: None,
    }
}

/// `OperationSql` wrapper for [`TSTZ_ARRAY_HELPER_PRELUDE`].
/// Same insertion discipline as [`date_array_helper_operation`].
pub(crate) fn tstz_array_helper_operation() -> OperationSql {
    OperationSql {
        label: "Ensure djogi timestamptz-array finite-element helper".to_string(),
        up: TSTZ_ARRAY_HELPER_PRELUDE.to_string(),
        down: "-- no-op rollback placeholder: helper is shared by framework CHECK constraints"
            .to_string(),
        lossy: None,
    }
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
/// The prior `promote_tmp` was not restoration-safe on overwrite: a
/// `fs::rename` over an existing file silently replaced the content,
/// and the rollback path could only `remove_file(final_path)`
/// losing the original bytes entirely. The new shape:
/// 1. If `final_path` already exists, copy its bytes into a sibling
///    `<final>.bak.<pid>.<counter>` backup. The counter is per-
///    process atomic so two simultaneous promotes never collide.
/// 2. Rename `tmp` over `final_path`.
/// 3. Return the backup path so the caller can hand it to the
///    [`WriteRollback`] guard for restoration on failure.
///    Returns `Ok(None)` when no prior file existed at `final_path`
///    (fresh create — nothing to back up). Returns `Ok(Some(path))` when
///    a backup was captured. Returns `Err` only if either I/O step
///    fails; in that case any partial backup is removed before
///    surfacing the error so the workspace is left clean.
fn promote_tmp_with_backup(tmp: &Path, final_path: &Path) -> Result<Option<PathBuf>, ComposeError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(0);

    // Canonicalize both paths and verify they share the same parent.
    // This prevents symlink-based path traversal: the tmp file was
    // created as a sibling of final_path in atomic_write, so after
    // canonicalization they must still reside in the same directory.
    let tmp_canonical = tmp.canonicalize().map_err(|e| ComposeError::Io {
        path: tmp.to_path_buf(),
        source: e,
    })?;
    let final_parent = final_path.parent().ok_or_else(|| ComposeError::Io {
        path: final_path.to_path_buf(),
        source: io::Error::other("final_path has no parent directory"),
    })?;
    // Canonicalize the parent (the directory exists; ensure_parent
    // was called before atomic_write). If final_path itself exists,
    // canonicalize it too to catch symlinked files.
    let final_parent_canonical = final_parent.canonicalize().map_err(|e| ComposeError::Io {
        path: final_parent.to_path_buf(),
        source: e,
    })?;
    if tmp_canonical.parent() != Some(&final_parent_canonical) {
        return Err(ComposeError::Io {
            path: tmp.to_path_buf(),
            source: io::Error::other(
                "tmp and final_path do not share the same parent directory after canonicalization",
            ),
        });
    }

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

fn check_pending_path_compatible(
    workspace_root: &Path,
    pending_path: &Path,
    bucket: &BucketKey,
) -> Result<(), ComposeError> {
    if !pending_path.exists() {
        return Ok(());
    }
    let pending_root = pending_root(workspace_root);
    let pending_root = pending_root.canonicalize().map_err(|e| ComposeError::Io {
        path: pending_root,
        source: e,
    })?;
    // The pending_root directory itself may not exist yet (first compose),
    // but the file does exist (we just checked). Canonicalize the closest
    // existing ancestor to validate containment.
    common::ensure_within_base(&pending_root, pending_path).map_err(|e| ComposeError::Io {
        path: pending_path.to_path_buf(),
        source: e,
    })?;
    let pending =
        load_pending(pending_path).map_err(|e| ComposeError::PendingJsonWouldBeOverwritten {
            path: pending_path.to_path_buf(),
            text: format!(
                "pending JSON would be overwritten at {}: existing file is not a compatible pending artifact ({e})",
                pending_path.display()
            ),
        })?;
    let same_bucket =
        pending.bucket_database == bucket.database && pending.bucket_app == bucket.app;
    let is_legacy_phase_zero =
        bucket.app.is_empty() && pending.version == super::bootstrap::PHASE_ZERO_VERSION;
    if same_bucket && !is_legacy_phase_zero {
        return Ok(());
    }
    Err(ComposeError::PendingJsonWouldBeOverwritten {
        path: pending_path.to_path_buf(),
        text: format!(
            "pending JSON would be overwritten at {}: existing file belongs to a different pending authority",
            pending_path.display()
        ),
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
    use crate::migrate::replay_plan::{self, ReplayPlanLoadStatus};
    use crate::migrate::schema::{
        ColumnSchema, EnumSchema, PkKindSchema, PrimaryKeySchema, SNAPSHOT_FORMAT_VERSION,
        TableSchema,
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
        crate::migrate::common::create_workspace_dir_all(&std::env::temp_dir(), &p).unwrap();
        p
    }

    fn cleanup_workspace(work: &Path) {
        let _ = crate::migrate::common::remove_workspace_dir_all(&std::env::temp_dir(), work);
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
                    codec: None,
                    comment: None,
                    default_sql: Some("heerid_next_desc()".to_string()),
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
                    type_change_using: None,
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
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            },
        );
        s
    }

    fn id_column_heerid_desc() -> ColumnSchema {
        ColumnSchema {
            default_sql: Some("heerid_next_desc()".to_string()),
            ..col("id", "BIGINT", false)
        }
    }

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnSchema {
        ColumnSchema {
            check: None,
            codec: None,
            comment: None,
            default_sql: None,
            foreign_key: None,
            generated: None,
            identity: None,
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
            sql_type: ty.to_string(),
            unique: false,
            type_change_using: None,
        }
    }

    fn col_numeric_array_metric_check() -> ColumnSchema {
        ColumnSchema {
            check: Some("djogi.__djogi_numeric_array_is_rust_decimal_v1(\"amounts\")".to_string()),
            codec: None,
            ..col("amounts", "NUMERIC[]", true)
        }
    }

    fn col_date_array_with_finite_check() -> ColumnSchema {
        ColumnSchema {
            check: Some("djogi.__djogi_date_array_is_finite_v1(\"blackout_dates\")".to_string()),
            codec: None,
            ..col("blackout_dates", "DATE[]", true)
        }
    }

    fn col_tstz_array_with_finite_check() -> ColumnSchema {
        ColumnSchema {
            check: Some("djogi.__djogi_tstz_array_is_finite_v1(\"scheduled_slots\")".to_string()),
            codec: None,
            ..col("scheduled_slots", "TIMESTAMPTZ[]", true)
        }
    }

    /// A table with all three array-helper column types: numeric, date, and
    /// timestamptz. Used by the mixed-helper checksum parity test.
    fn table_with_all_three_array_helpers(bucket: &BucketKey) -> TableSchema {
        TableSchema {
            app: if bucket.app.is_empty() {
                None
            } else {
                Some(bucket.app.clone())
            },
            columns: vec![
                id_column_heerid_desc(),
                col_numeric_array_metric_check(),
                col_date_array_with_finite_check(),
                col_tstz_array_with_finite_check(),
            ],
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
            table: "mixed_array_events".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    fn table_with_numeric_array_metric_check(bucket: &BucketKey) -> TableSchema {
        TableSchema {
            app: if bucket.app.is_empty() {
                None
            } else {
                Some(bucket.app.clone())
            },
            columns: vec![id_column_heerid_desc(), col_numeric_array_metric_check()],
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
            table: "metrics_events".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    fn global_bucket() -> BucketKey {
        BucketKey {
            database: "main".into(),
            app: "".into(),
        }
    }

    #[test]
    fn compose_pending_checksum_matches_runner_plan_checksum_for_numeric_array_helper_migrations() {
        let work = temp_workspace("numeric-array-helper-checksum");
        let guard = lock_for(&work);
        let bucket = global_bucket();

        let mut models = BTreeMap::new();
        let mut model_snapshot = snapshot_with_widgets(&bucket);
        model_snapshot.models.insert(
            "metrics_events".to_string(),
            table_with_numeric_array_metric_check(&bucket),
        );
        models.insert(bucket.clone(), model_snapshot);

        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "numeric-array-checksum-parity",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("compose should succeed");
        assert_eq!(report.composed_buckets.len(), 1);

        let pending_bytes = crate::migrate::read_workspace_file(
            &work,
            &report.composed_buckets[0].pending_json_path,
        )
        .unwrap();
        let pending: PendingPlan = serde_json::from_slice(&pending_bytes).expect("parse pending");

        let deltas = diff_bucket_maps(&snapshots, &models).expect("diff for expected checksum");
        let delta = deltas
            .into_iter()
            .find(|delta| delta.bucket == bucket)
            .expect("numeric-array bucket delta");
        let plan = plan_delta(&delta).expect("canonical plan for runner-style checksum");
        let runner_style_checksum = compute_checksum(
            plan.segments
                .iter()
                .flat_map(|segment| segment.statements.iter())
                .map(|statement| statement.up.as_str()),
        );

        assert_eq!(
            pending.checksum_up, runner_style_checksum,
            "compose pending checksum must match runner-plan checksum when NumericArray helper is injected"
        );
        cleanup_workspace(&work);
    }

    #[test]
    fn compose_pending_checksum_matches_runner_plan_checksum_for_mixed_helper_delta() {
        // Regression guard: when a delta requires all three array helpers
        // (numeric, date, tstz), the checksum stored in the pending JSON by
        // `compose` must equal the checksum the runner derives from
        // `plan_delta` independently. Both paths must agree on which ops
        // are included and in which order — a divergence would cause the
        // runner to reject the migration with a checksum mismatch.
        // Note: both `compose` and `runner` derive their checksum from the
        // same `plan_delta` output, so this test also guards against a
        // regression where `compose` accidentally computes the checksum from
        // the un-augmented `lowered` ops (i.e. without helper preludes).
        let work = temp_workspace("mixed-array-helper-checksum");
        let guard = lock_for(&work);
        let bucket = global_bucket();

        let mut models = BTreeMap::new();
        let mut model_snapshot = snapshot_with_widgets(&bucket);
        model_snapshot.models.insert(
            "mixed_array_events".to_string(),
            table_with_all_three_array_helpers(&bucket),
        );
        models.insert(bucket.clone(), model_snapshot);

        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "mixed-array-checksum-parity",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("compose should succeed");
        assert_eq!(report.composed_buckets.len(), 1);

        let pending_bytes = crate::migrate::read_workspace_file(
            &work,
            &report.composed_buckets[0].pending_json_path,
        )
        .unwrap();
        let pending: PendingPlan = serde_json::from_slice(&pending_bytes).expect("parse pending");

        let deltas = diff_bucket_maps(&snapshots, &models).expect("diff for expected checksum");
        let delta = deltas
            .into_iter()
            .find(|d| d.bucket == bucket)
            .expect("mixed-array bucket delta");
        let plan = plan_delta(&delta).expect("canonical plan for runner-style checksum");
        let runner_style_checksum = compute_checksum(
            plan.segments
                .iter()
                .flat_map(|segment| segment.statements.iter())
                .map(|statement| statement.up.as_str()),
        );

        assert_eq!(
            pending.checksum_up, runner_style_checksum,
            "compose pending checksum must match runner-plan checksum when all three \
             array helpers (numeric, date, tstz) are injected"
        );

        // Additionally verify that the three helpers appear in the on-disk SQL
        // file in compose order (numeric → date → tstz). The SQL file is the
        // operator-visible artifact and must reflect actual execution order.
        let up_sql = crate::migrate::read_workspace_file_to_string(
            &work,
            &report.composed_buckets[0].up_sql_path,
        )
        .expect("read up SQL");
        let numeric_sql_pos = up_sql
            .find("__djogi_numeric_array_is_rust_decimal_v1")
            .expect("numeric helper in SQL file");
        let date_sql_pos = up_sql
            .find("__djogi_date_array_is_finite_v1")
            .expect("date helper in SQL file");
        let tstz_sql_pos = up_sql
            .find("__djogi_tstz_array_is_finite_v1")
            .expect("tstz helper in SQL file");
        assert!(
            numeric_sql_pos < date_sql_pos,
            "numeric helper prelude must precede date helper prelude in SQL file"
        );
        assert!(
            date_sql_pos < tstz_sql_pos,
            "date helper prelude must precede tstz helper prelude in SQL file"
        );
        cleanup_workspace(&work);
    }

    #[test]
    fn fallback_plan_round_trips_rendered_helper_prelude_files() {
        let work = temp_workspace("numeric-array-helper-fallback");
        let guard = lock_for(&work);
        let bucket = global_bucket();

        let mut models = BTreeMap::new();
        let mut model_snapshot = snapshot_with_widgets(&bucket);
        model_snapshot.models.insert(
            "metrics_events".to_string(),
            table_with_numeric_array_metric_check(&bucket),
        );
        models.insert(bucket.clone(), model_snapshot);

        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), empty_snapshot(&bucket));

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &[],
            name: "numeric-array-fallback-round-trip",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("compose should succeed");
        assert_eq!(report.composed_buckets.len(), 1);
        let composed = &report.composed_buckets[0];

        let pending: PendingPlan = serde_json::from_slice(
            &crate::migrate::read_workspace_file(&work, &composed.pending_json_path).unwrap(),
        )
        .expect("parse pending");

        // Read the RENDERED files back from disk — the builder input is
        // the committed SQL text, never the sidecar (this is the no-sidecar
        // path; the sidecar compose wrote is deliberately ignored here).
        let up_sql =
            crate::migrate::read_workspace_file_to_string(&work, &composed.up_sql_path).unwrap();
        let down_sql =
            crate::migrate::read_workspace_file_to_string(&work, &composed.down_sql_path).unwrap();
        assert!(
            up_sql.contains(NUMERIC_ARRAY_HELPER_PRELUDE),
            "fixture must carry the injected helper prelude: {up_sql}"
        );

        let built = crate::migrate::canonical_fallback_replay_plan(
            &bucket,
            &pending.version,
            &up_sql,
            &down_sql,
        )
        .expect("rendered helper-prelude files must build a fallback plan");

        // Builder values == the pending/sidecar values, byte for byte.
        assert_eq!(built.checksum_up, pending.checksum_up);
        assert_eq!(built.checksum_down, pending.checksum_down);
        assert!(built.checksum_down.is_some());

        // Runner-verification invariant: the recovered plan rehashes to
        // the pending checksum, helper prelude included (mirrors
        // compute_checksum_for_plan_up).
        let rehash = compute_checksum(
            built
                .plan
                .segments
                .iter()
                .flat_map(|segment| segment.statements.iter())
                .map(|statement| statement.up.as_str()),
        );
        assert_eq!(rehash, pending.checksum_up);

        // The prelude is recovered as the leading executable statement.
        assert_eq!(
            built.plan.segments[0].statements[0].up,
            NUMERIC_ARRAY_HELPER_PRELUDE
        );
        cleanup_workspace(&work);
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("noop");
        assert!(matches!(err, ComposeError::NothingToCompose));
        cleanup_workspace(&work);
    }

    #[test]
    fn add_table_writes_four_files_atomically() {
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("compose");
        assert_eq!(report.composed_buckets.len(), 1);
        let cb = &report.composed_buckets[0];
        assert!(cb.up_sql_path.exists());
        assert!(cb.down_sql_path.exists());
        assert!(cb.replay_plan_path.exists());
        assert!(cb.pending_json_path.exists());
        // Up SQL must contain CREATE TABLE.
        let up = crate::migrate::read_workspace_file_to_string(&work, &cb.up_sql_path).unwrap();
        assert!(up.contains("CREATE TABLE \"widgets\""));
        // Pending JSON must round-trip through PendingPlan.
        let pending_bytes =
            crate::migrate::read_workspace_file(&work, &cb.pending_json_path).unwrap();
        let pending: PendingPlan = serde_json::from_slice(&pending_bytes).expect("parse");
        match replay_plan::load_committed_replay_plan(
            &work,
            &cb.bucket,
            &cb.version,
            &pending.checksum_up,
            pending.checksum_down.as_deref(),
        ) {
            ReplayPlanLoadStatus::Loaded(plan) => {
                assert_eq!(plan.classification, Classification::Additive);
                assert_eq!(plan.segments.len(), 1);
                assert_eq!(plan.segments[0].kind, SegmentKind::Transactional);
                assert_eq!(plan.segments[0].statements[0].label, "AddTable widgets");
            }
            other => panic!("expected committed replay plan, got {other:?}"),
        }
        assert_eq!(pending.bucket_app, "");
        assert_eq!(pending.bucket_database, "main");
        assert!(pending.checksum_up.starts_with("V1:"));
        assert!(pending.version.starts_with("V20260425010203__"));
        cleanup_workspace(&work);
    }

    #[test]
    fn destructive_classification_requires_allow_destructive() {
        // Snapshot has widgets+gadgets, models only have gadgets.
        // DROP widgets is destructive. Linkage guard passes because
        // models still project gadgets (non-zero models for bucket).
        let work = temp_workspace("destructive");
        let guard = lock_for(&work);
        let bucket = global_bucket();

        // Snapshot with two tables: widgets + gadgets
        let mut snap = snapshot_with_widgets(&bucket);
        snap.models.insert(
            "gadgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![id_column_heerid_desc()],
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
                table: "gadgets".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            },
        );

        // Models only have gadgets (not widgets) -> DROP widgets delta
        let mut models = BTreeMap::new();
        let mut model_snap = snapshot_with_widgets(&bucket);
        model_snap.models.remove("widgets");
        model_snap.models.insert(
            "gadgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![id_column_heerid_desc()],
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
                table: "gadgets".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            },
        );
        models.insert(bucket.clone(), model_snap);

        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), snap);

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("destructive");
        assert!(matches!(
            err,
            ComposeError::DestructiveRequiresAllowDestructive { .. }
        ));
        // No file should have been written.
        let dir = bucket_dir(&work, &bucket);
        let count = crate::migrate::read_workspace_dir(&work, &dir)
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(count, 0, "no SQL written on destructive refusal");
        cleanup_workspace(&work);
    }

    #[test]
    fn destructive_with_allow_destructive_writes_files() {
        // Snapshot has widgets+gadgets, models only have gadgets.
        // DROP widgets is destructive. Linkage guard passes because
        // models still project gadgets (non-zero models for bucket).
        let work = temp_workspace("destructive_ok");
        let guard = lock_for(&work);
        let bucket = global_bucket();

        // Snapshot with two tables: widgets + gadgets
        let mut snap = snapshot_with_widgets(&bucket);
        snap.models.insert(
            "gadgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![id_column_heerid_desc()],
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
                table: "gadgets".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            },
        );

        // Models only have gadgets -> DROP widgets delta (destructive)
        let mut models = BTreeMap::new();
        let mut model_snap = snapshot_with_widgets(&bucket);
        model_snap.models.remove("widgets");
        model_snap.models.insert(
            "gadgets".to_string(),
            TableSchema {
                app: None,
                columns: vec![id_column_heerid_desc()],
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
                table: "gadgets".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            },
        );
        models.insert(bucket.clone(), model_snap);

        let mut snapshots = BTreeMap::new();
        snapshots.insert(bucket.clone(), snap);

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("compose");
        assert_eq!(report.composed_buckets.len(), 1);
        cleanup_workspace(&work);
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
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
        cleanup_workspace(&work);
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
            codec: None,
            comment: None,
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
            type_change_using: None,
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
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
        let up = crate::migrate::read_workspace_file_to_string(&work, &dest.up_sql_path).unwrap();
        assert!(
            up.contains("RenameApp"),
            "up SQL must label the RenameApp op: {up}"
        );
        assert!(up.contains("oldname"));
        assert!(up.contains("newname"));
        cleanup_workspace(&work);
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first");
        let up1 = crate::migrate::read_workspace_file(&work, &r1.composed_buckets[0].up_sql_path)
            .unwrap();
        let pending1 =
            crate::migrate::read_workspace_file(&work, &r1.composed_buckets[0].pending_json_path)
                .unwrap();
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let r2 = compose(req2).expect("second");
        let up2 = crate::migrate::read_workspace_file(&work, &r2.composed_buckets[0].up_sql_path)
            .unwrap();
        let pending2 =
            crate::migrate::read_workspace_file(&work, &r2.composed_buckets[0].pending_json_path)
                .unwrap();
        assert_eq!(up1, up2, "up SQL must be byte-identical");
        assert_eq!(pending1, pending2, "pending JSON must be byte-identical");
        cleanup_workspace(&work);
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
            depends_on: Vec::new(),
        };
        let bytes = serde_json::to_vec(&plan).unwrap();
        let parsed: PendingPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, plan);
    }

    // ── Fixup regression coverage ─────────────────────────────────────

    /// D011 fires when a tombstoned app has zero current models but
    /// the snapshot still carries schema state to drop. Prior to the
    /// fix the `!s.models.is_empty` guard skipped this path and the
    /// operator only saw the generic destructive classification error.
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
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
        cleanup_workspace(&work);
    }

    /// Second compose with the SAME inputs but a hand edit to the up
    /// SQL file refuses with D013 (no `--force-overwrite`). With
    /// `force_overwrite = true` the same scenario succeeds and the
    /// edits are discarded.
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first");
        let up_path = r1.composed_buckets[0].up_sql_path.clone();
        // Operator hand-edits the up SQL.
        let original = crate::migrate::read_workspace_file_to_string(&work, &up_path).unwrap();
        let edited = original.clone() + "\n-- operator hand-edit\n";
        crate::migrate::write_workspace_file(&work, &up_path, edited.as_bytes()).unwrap();

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("must refuse");
        match err {
            ComposeError::HandEditedMigrationWouldBeOverwritten { text, path, .. } => {
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
        let after_refusal = crate::migrate::read_workspace_file_to_string(&work, &up_path).unwrap();
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        compose(req3).expect("force-overwrite succeeds");
        let after_force = crate::migrate::read_workspace_file_to_string(&work, &up_path).unwrap();
        assert_eq!(
            after_force, original,
            "force-overwrite must restore canonical SQL"
        );
        cleanup_workspace(&work);
    }

    /// Round-trip rename app. Compose with `renamed_from = "oldname"`
    /// on the new bucket must:
    /// 1. Emit `UPDATE djogi_schema_migrations SET app_label =
    /// 'newname' WHERE app_label = 'oldname';` into the up SQL.
    /// 2. Emit the inverse UPDATE into the down SQL.
    /// 3. Move `migrations/main/oldname/` → `migrations/main/newname/`
    ///    on disk.
    /// 4. Succeed WITHOUT `--allow-destructive`. The on-disk SQL
    ///    tables don't move when an app renames;
    ///    `remap_snapshots_for_renames` relabels the OLD-bucket
    ///    snapshot under NEW before diffing so no DropTable /
    ///    AddTable pair appears, and the classification stays
    ///    metadata-only.
    /// 5. The SQL must NOT carry a DROP TABLE for the renamed-from
    ///    bucket's tables — they aren't being dropped.
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
        crate::migrate::create_workspace_dir_all(&work, &old_dir).unwrap();
        crate::migrate::write_workspace_file(
            &work,
            old_dir.join("V20260101010101__init.sdjql"),
            b"-- init",
        )
        .unwrap();
        // Save the snapshot at the old bucket. The CLI reads this;
        // the lib-side test passes it through `snapshots`.
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
            // destructive opt-in.
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("compose");
        let dest = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == new_bucket)
            .expect("destination composed");
        let up = crate::migrate::read_workspace_file_to_string(&work, &dest.up_sql_path).unwrap();
        let down =
            crate::migrate::read_workspace_file_to_string(&work, &dest.down_sql_path).unwrap();
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
        assert!(new_dir.join("V20260101010101__init.sdjql").exists());
        // 5.
        // TABLE for `widgets` — the table isn't being dropped, just
        // re-labelled at the app boundary.
        assert!(
            !up.contains("DROP TABLE \"widgets\""),
            "rename must not emit DROP TABLE for widgets: {up}"
        );
        cleanup_workspace(&work);
    }

    /// Pending JSON with future `format_version` surfaces
    /// `UnsupportedFormatVersion` from [`parse_pending_bytes`] BEFORE
    /// the structural deserialize trips on extra fields. The production
    /// build.rs reader mirrors this peek pattern (see
    /// `b7_pending_format_version_peek_present` in the agreement
    /// integration test).
    #[test]
    fn b7_pending_format_version_peek_rejects_future_version() {
        let blob = r#"{
            "format_version": "3",
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
            "future_field_added_in_v3": "garbage"
        }"#;
        let err = parse_pending_bytes(blob.as_bytes(), None).expect_err("must fail");
        match err {
            PendingLoadError::UnsupportedFormatVersion {
                found, expected, ..
            } => {
                assert_eq!(found, "3");
                assert_eq!(expected, PENDING_FORMAT_VERSION);
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
            depends_on: Vec::new(),
        };
        let bytes = serde_json::to_vec(&plan).unwrap();
        let parsed = parse_pending_bytes(&bytes, None).expect("loader accepts canonical shape");
        assert_eq!(parsed, plan);
    }

    /// A pre-#398 format-"1" pending file must be rejected with the
    /// actionable version-mismatch error (telling the operator to
    /// recompose), NOT a generic serde unknown/missing-field error.
    /// The blob below is the shape an older djogi binary produced:
    /// `format_version = "1"` and no `depends_on` field.
    #[test]
    fn format_one_pending_rejected_with_version_mismatch() {
        let blob = r#"{
            "format_version": "1",
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
                "registered_apps": ["billing"]
            },
            "checksum_up": "V1:0000000000000000000000000000000000000000000000000000000000000000",
            "checksum_down": null,
            "composed_at": "2026-04-25T01:02:03Z"
        }"#;
        let err =
            parse_pending_bytes(blob.as_bytes(), None).expect_err("old format must be rejected");
        assert!(
            matches!(err, PendingLoadError::UnsupportedFormatVersion { .. }),
            "expected the actionable upgrade error, got {err:?}"
        );
    }

    /// A stale pending file (numeric `found` below the expected version)
    /// must tell the operator to recompose — the file was produced by an
    /// older djogi and the current binary can regenerate it.
    #[test]
    fn unsupported_format_version_display_stale_says_recompose() {
        let err = PendingLoadError::UnsupportedFormatVersion {
            found: "1".to_string(),
            expected: PENDING_FORMAT_VERSION,
            path: None,
        };
        let msg = err.to_string();
        assert!(
            msg.ends_with("; re-run 'djogi migrations compose' to regenerate this pending file"),
            "stale must end with '; <recompose phrase>': {msg}"
        );
    }

    /// A future pending file (numeric `found` above the expected version)
    /// must tell the operator to upgrade djogi — recomposing with the
    /// current binary would only downgrade the file. The path-bearing arm
    /// must still name the offending file.
    #[test]
    fn unsupported_format_version_display_future_says_upgrade() {
        let err = PendingLoadError::UnsupportedFormatVersion {
            found: "3".to_string(),
            expected: PENDING_FORMAT_VERSION,
            path: Some(std::path::PathBuf::from("migrations/main/_global_/V.json")),
        };
        let msg = err.to_string();
        assert!(
            msg.ends_with("; upgrade to a newer version of djogi (or check out a newer revision)"),
            "future must end with '; <upgrade phrase>': {msg}"
        );
        assert!(
            msg.contains("migrations/main/_global_/V.json"),
            "path arm must still name the file: {msg}"
        );
    }

    /// A non-numeric `found` (e.g. a hand-edited `v2-beta`) cannot be
    /// ordered against the expected version, so the message falls back to
    /// the generic upgrade hint rather than panicking on the parse.
    #[test]
    fn unsupported_format_version_display_non_numeric_fallback() {
        let err = PendingLoadError::UnsupportedFormatVersion {
            found: "v2-beta".to_string(),
            expected: PENDING_FORMAT_VERSION,
            path: None,
        };
        let msg = err.to_string();
        assert!(
            msg.ends_with("; upgrade or check out a newer djogi"),
            "non-numeric must end with '; <fallback phrase>': {msg}"
        );
    }

    /// Rollback guard removes ALL staged tmp files when any rename in
    /// the dance fails. We simulate this by pre-creating the down_path
    /// as a directory (which makes the down rename fail with
    /// `IsADirectory`); the guard must remove the up tmp, the down tmp,
    /// and the pending tmp, plus roll back the up rename.
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
        crate::migrate::create_workspace_dir_all(&work, &bucket_directory).unwrap();
        let blocked_down = bucket_directory.join(&down_filename_str);
        crate::migrate::create_workspace_dir_all(&work, &blocked_down).unwrap();
        // Drop a sentinel so removing the directory would matter.
        crate::migrate::write_workspace_file(&work, blocked_down.join("sentinel"), b"keep")
            .unwrap();

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("rename must fail");
        assert!(matches!(err, ComposeError::Io { .. }));

        // Now verify the workspace is clean: zero `<*>.tmp.<pid>`
        // files anywhere, and the up SQL was rolled back. The
        // pre-existing blocking directory is intentionally untouched.
        let mut tmp_files = Vec::new();
        if let Ok(entries) = crate::migrate::read_workspace_dir(&work, &bucket_directory) {
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
        cleanup_workspace(&work);
    }

    /// `WriteRollback` must restore original bytes when a tmp was
    /// promoted OVER an existing file. We simulate a mid-sequence
    /// failure by:
    /// 1. Pre-creating the up SQL file with content `"old"` (so the
    ///    up promote is an OVERWRITE, not a fresh create).
    /// 2. Pre-creating the down_path as a directory so the down
    ///    promote fails. The up promote has already succeeded by
    ///    that point, so its rollback path runs.
    ///    Asserts:
    /// - tmp files cleaned up (contract still holds).
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
        crate::migrate::create_workspace_dir_all(&work, &bucket_directory).unwrap();
        // Pre-existing up SQL — operator's prior content. The
        // promote will overwrite this; the rollback must restore it.
        let up_path = bucket_directory.join(up_filename(&version));
        crate::migrate::write_workspace_file(&work, &up_path, b"old up content").unwrap();
        // Block the down promote so the sequence fails after the up
        // promote has already overwritten the existing up file.
        let blocked_down = bucket_directory.join(down_filename(&version));
        crate::migrate::create_workspace_dir_all(&work, &blocked_down).unwrap();

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("down promote must fail");
        assert!(matches!(err, ComposeError::Io { .. }));

        // (a) tmp files cleaned up.
        let mut tmp_files: Vec<String> = Vec::new();
        if let Ok(entries) = crate::migrate::read_workspace_dir(&work, &bucket_directory) {
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
        let after = crate::migrate::read_workspace_file_to_string(&work, &up_path)
            .expect("up still exists");
        assert_eq!(
            after, "old up content",
            "rollback must restore original up bytes from the backup"
        );

        // (c) No `.bak.<pid>.<n>` files remain anywhere in the
        // bucket directory.
        let mut bak_files: Vec<String> = Vec::new();
        if let Ok(entries) = crate::migrate::read_workspace_dir(&work, &bucket_directory) {
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
        cleanup_workspace(&work);
    }

    /// `WriteRollback` must restore BOTH the up and the down bytes
    /// when a mid-sequence failure occurs after MULTIPLE promotes have
    /// already overwritten existing files.
    /// The original test (above) exercises a single restore point
    /// the down promote fails so only the up rollback is tested.
    /// This sibling test stresses the LIFO unwind in
    /// [`WriteRollback::drop`]: it forces the failure at the THIRD
    /// promote (replay plan), so up + down promotes have already
    /// captured backups and the rollback must restore each in reverse
    /// order.
    /// Strategy:
    /// 1. Pre-create up SQL with "operator up content".
    /// 2. Pre-create down SQL with "operator down content".
    /// 3. Block the replay-plan promote by creating its target as a
    ///    NON-EMPTY directory (so `fs::rename(<file>, <non-empty-dir>)`
    ///    fails with a kernel-level error). The replay-plan sidecar
    ///    lives alongside the SQL files, after the up/down promotes,
    ///    and is not preflighted by the pending-authority guard.
    ///    Asserts:
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
        crate::migrate::create_workspace_dir_all(&work, &bucket_directory).unwrap();

        // (1) + (2) — pre-existing operator content on BOTH SQL files.
        // Each promote will overwrite these; the rollback must restore
        // each one back to its original bytes via the LIFO unwind.
        let up_path = bucket_directory.join(up_filename(&version));
        let down_path = bucket_directory.join(down_filename(&version));
        let original_up = b"operator up content";
        let original_down = b"operator down content";
        crate::migrate::write_workspace_file(&work, &up_path, original_up).unwrap();
        crate::migrate::write_workspace_file(&work, &down_path, original_down).unwrap();

        // (3) — block the THIRD promote (replay-plan sidecar) by
        // pre-creating its destination as a non-empty directory.
        let replay_plan_path = committed_replay_plan_path(&work, &bucket, &version);
        crate::migrate::create_workspace_dir_all(&work, &replay_plan_path).unwrap();
        crate::migrate::write_workspace_file(&work, replay_plan_path.join("sentinel"), b"keep")
            .unwrap();

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("replay-plan promote must fail");
        assert!(
            matches!(err, ComposeError::Io { .. }),
            "must surface a typed I/O error: {err:?}"
        );

        // (a) BOTH up and down files restored to their original
        // operator content. The LIFO unwind in
        // `WriteRollback::drop` runs the down restore first, then
        // the up restore — but we only observe the final state,
        // which must match the pre-compose state byte-for-byte.
        let after_up =
            crate::migrate::read_workspace_file(&work, &up_path).expect("up file still present");
        assert_eq!(
            after_up.as_slice(),
            original_up,
            "up file must be restored to original operator content"
        );
        let after_down = crate::migrate::read_workspace_file(&work, &down_path)
            .expect("down file still present");
        assert_eq!(
            after_down.as_slice(),
            original_down,
            "down file must be restored to original operator content"
        );

        // (b) No `.tmp.<pid>.<n>` files remain in the bucket directory.
        let mut tmp_files: Vec<String> = Vec::new();
        if let Ok(entries) = crate::migrate::read_workspace_dir(&work, &bucket_directory) {
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
        // directory. The LIFO restore renames each backup back
        // over its final path, leaving zero backup siblings.
        let mut bak_files: Vec<String> = Vec::new();
        if let Ok(entries) = crate::migrate::read_workspace_dir(&work, &bucket_directory) {
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
        cleanup_workspace(&work);
    }

    /// D013 also fires when ONLY the down SQL was hand-edited. The
    /// original test only covered the up side; a later round caught the
    /// down side as silently overwriteable.
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first compose");
        let down_path = r1.composed_buckets[0].down_sql_path.clone();
        let original_down =
            crate::migrate::read_workspace_file_to_string(&work, &down_path).unwrap();
        let edited_down = original_down.clone() + "\n-- operator hand-edit on down only\n";
        crate::migrate::write_workspace_file(&work, &down_path, edited_down.as_bytes()).unwrap();

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("down hand-edit must refuse");
        match err {
            ComposeError::HandEditedMigrationWouldBeOverwritten { text, path, .. } => {
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
        let after = crate::migrate::read_workspace_file_to_string(&work, &down_path).unwrap();
        assert_eq!(after, edited_down);
        cleanup_workspace(&work);
    }

    /// D013 fires when BOTH up and down were edited. The diagnostic
    /// surfaces both via the side label.
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let r1 = compose(req1).expect("first compose");
        let up_path = r1.composed_buckets[0].up_sql_path.clone();
        let down_path = r1.composed_buckets[0].down_sql_path.clone();
        crate::migrate::write_workspace_file(&work, &up_path, b"-- hand edit up\n").unwrap();
        crate::migrate::write_workspace_file(&work, &down_path, b"-- hand edit down\n").unwrap();

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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("both-side edit must refuse");
        match err {
            ComposeError::HandEditedMigrationWouldBeOverwritten { text, path, .. } => {
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
        cleanup_workspace(&work);
    }

    /// Rename app with multiple existing tables must succeed WITHOUT
    /// `--allow-destructive`. This guards the snapshot-key remap step
    /// in `remap_snapshots_for_renames`: if the remap regresses, the
    /// differ would emit DropTable for each of the OLD bucket's three
    /// tables and the test would fail with
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
                            codec: None,
                            comment: None,
                            default_sql: Some("heerid_next_desc()".to_string()),
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
                            type_change_using: None,
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
                        table_comment: None,
                        storage_params: None,
                        tablespace: None,
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("rename without --allow-destructive must succeed");
        let dest = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == new_bucket)
            .expect("destination bucket composed");
        let up = crate::migrate::read_workspace_file_to_string(&work, &dest.up_sql_path).unwrap();
        // No DropTable for any of the three table names.
        for name in ["invoices", "customers", "line_items"] {
            let drop_text = format!("DROP TABLE \"{name}\"");
            assert!(
                !up.contains(&drop_text),
                "rename must not emit {drop_text}: {up}"
            );
        }
        // The RenameApp ledger UPDATE is still there.
        assert!(up.contains("UPDATE djogi_schema_migrations"));
        cleanup_workspace(&work);
    }

    /// `rename_old_bucket_folder` refuses fail-fast when the
    /// destination directory already contains an entry colliding with
    /// the OLD directory's content. The prior shape silently skipped
    /// collisions (dropping the OLD entry); the new shape returns a
    /// typed `FolderRenameTargetCollision` error before any move
    /// happens.
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
        crate::migrate::create_workspace_dir_all(&work, &old_dir).unwrap();
        crate::migrate::create_workspace_dir_all(&work, &new_dir).unwrap();
        // Both directories contain a file of the SAME name with
        // DIFFERENT content — a collision the prior merge loop would
        // silently swallow.
        crate::migrate::write_workspace_file(
            &work,
            old_dir.join("V20260101010101__init.sdjql"),
            b"from-old",
        )
        .unwrap();
        crate::migrate::write_workspace_file(
            &work,
            new_dir.join("V20260101010101__init.sdjql"),
            b"from-new",
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
            // Existing compose unit tests target the delta-based
            // write/rollback machinery in isolation. Bootstrap auto-emit
            // is exercised by dedicated integration + unit tests; opt
            // out here so the per-bucket directory assertions stay
            // tight to what these tests actually verify.
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req).expect_err("collision must surface");
        match err {
            ComposeError::FolderRenameTargetCollision {
                offending_entry, ..
            } => {
                assert_eq!(offending_entry, "V20260101010101__init.sdjql");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // The pre-existing files are left untouched (no partial
        // merge state).
        assert_eq!(
            crate::migrate::read_workspace_file_to_string(
                &work,
                old_dir.join("V20260101010101__init.sdjql"),
            )
            .unwrap(),
            "from-old"
        );
        assert_eq!(
            crate::migrate::read_workspace_file_to_string(
                &work,
                new_dir.join("V20260101010101__init.sdjql"),
            )
            .unwrap(),
            "from-new"
        );
        cleanup_workspace(&work);
    }

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

    /// Testing-gap acknowledgement: the `WriteRollback.entry_renames`
    /// queue exists so a mid-loop failure during the post-compose
    /// folder merge unwinds every already-moved entry. In practice the
    /// pre-flight collision scan in `rename_old_bucket_folder` catches
    /// every deterministically-reachable conflict before any entry move
    /// runs — so the rollback path is unreachable from a unit test
    /// harness without monkey-patching `fs::rename` to fail
    /// mid-iteration.
    /// This test pins that observation: it constructs two distinct
    /// collision shapes (file-vs-file and file-vs-directory) and
    /// asserts the pre-flight surfaces a typed
    /// [`ComposeError::FolderRenameTargetCollision`] BEFORE any move
    /// happens. The OLD directory is left intact (the rollback queue
    /// would be irrelevant — pre-flight pre-empted it).
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
            crate::migrate::create_workspace_dir_all(&work, &old_dir).unwrap();
            crate::migrate::create_workspace_dir_all(&work, &new_dir).unwrap();
            // Two entries on the OLD side; the SECOND one collides on
            // the NEW side. If the pre-flight check were ever loosened
            // to skip later entries, the first move would land and the
            // second would fail mid-loop — which is the scenario we
            // want to make unreachable. Today the pre-flight inspects
            // every entry up-front and refuses fail-fast.
            crate::migrate::write_workspace_file(
                &work,
                old_dir.join("V20260101010101__a.sdjql"),
                b"movable",
            )
            .unwrap();
            crate::migrate::write_workspace_file(
                &work,
                old_dir.join("V20260101010102__b.sdjql"),
                b"from-old",
            )
            .unwrap();
            crate::migrate::write_workspace_file(
                &work,
                new_dir.join("V20260101010102__b.sdjql"),
                b"from-new",
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
            let err = compose(req).expect_err("collision must surface");
            match err {
                ComposeError::FolderRenameTargetCollision {
                    offending_entry, ..
                } => {
                    assert_eq!(offending_entry, "V20260101010102__b.sdjql");
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
                old_dir.join("V20260101010101__a.sdjql").exists(),
                "movable entry must remain under OLD — pre-flight \
                 must pre-empt the entire merge loop"
            );
            assert!(
                !new_dir.join("V20260101010101__a.sdjql").exists(),
                "movable entry must NOT have been promoted into NEW"
            );
            cleanup_workspace(&work);
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
            crate::migrate::create_workspace_dir_all(&work, &old_dir).unwrap();
            crate::migrate::create_workspace_dir_all(&work, &new_dir).unwrap();
            // OLD has a file at `V20260101010101__init.sdjql`. NEW has
            // a DIRECTORY at the same path. Without the pre-flight,
            // `fs::rename(<file>, <existing-dir>)` would fail
            // mid-loop with EISDIR.
            crate::migrate::write_workspace_file(
                &work,
                old_dir.join("V20260101010101__init.sdjql"),
                b"movable",
            )
            .unwrap();
            crate::migrate::create_workspace_dir_all(
                &work,
                new_dir.join("V20260101010101__init.sdjql"),
            )
            .unwrap();
            crate::migrate::write_workspace_file(
                &work,
                new_dir.join("V20260101010101__init.sdjql").join("sentinel"),
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
                    assert_eq!(offending_entry, "V20260101010101__init.sdjql");
                }
                other => panic!("wrong variant (file-vs-dir): {other:?}"),
            }
            // Sentinel inside the blocking directory survives — the
            // rollback never ran because pre-flight pre-empted it.
            assert!(
                new_dir
                    .join("V20260101010101__init.sdjql")
                    .join("sentinel")
                    .exists(),
                "blocking directory's contents must be preserved"
            );
            cleanup_workspace(&work);
        }
    }

    /// `remap_snapshots_for_renames` must rewrite the OLD bucket key
    /// AND the embedded `registered_apps` list on the relabeled
    /// snapshot, while leaving every other bucket in the input map
    /// untouched.
    /// The differ inspects `registered_apps` on the destination bucket
    /// for an "app move" consistency check. If the relabel only
    /// rewrote the BTreeMap key but left the embedded list pointing at
    /// the OLD label, the differ would see a mismatch where the new
    /// bucket's snapshot does not list itself as a registered app
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
        // under the same database; the OLD key no longer exists.
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
        // contains "invoicing" and does NOT contain "billing".
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
        // and value (including its registered_apps list).
        let after_audit = after.get(&audit_bucket).expect("audit untouched");
        assert_eq!(*after_audit, before_audit);
    }

    #[test]
    fn compose_up_down_prepends_numeric_array_helper_once_when_check_is_referenced() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".into(),
                app: "billing".into(),
            },
            operations: Vec::new(),
            classification: Classification::Reversible,
        };
        let lowered = vec![OperationSql {
            label: "add metric check".into(),
            up: r#"ALTER TABLE "invoices" ADD CONSTRAINT "metrics_check" CHECK (djogi.__djogi_numeric_array_is_rust_decimal_v1("metrics"));"#
                .into(),
            down: r#"ALTER TABLE "invoices" DROP CONSTRAINT "metrics_check";"#
                .into(),
            lossy: None,
        }];
        let version = "V20260518__numeric_array_helper";
        let up_sql = compose_up_text(version, &delta, &lowered);
        let down_sql = compose_down_text(version, &delta, &lowered);

        // Prelude is anchored once in each side, before the first
        // operation comment so a downstream operator can execute the
        // file without scanning labels for required helper dependencies.
        assert!(
            up_sql.contains(NUMERIC_ARRAY_HELPER_PRELUDE),
            "Up migration must include numeric helper prelude: {up_sql}"
        );
        assert!(
            down_sql.contains(NUMERIC_ARRAY_HELPER_PRELUDE),
            "Down migration must include numeric helper prelude: {down_sql}"
        );
        assert_eq!(
            up_sql.matches("CREATE SCHEMA IF NOT EXISTS djogi;").count(),
            1,
            "Up migration must emit helper prelude once: {up_sql}"
        );
        assert_eq!(
            down_sql
                .matches("CREATE SCHEMA IF NOT EXISTS djogi;")
                .count(),
            1,
            "Down migration must emit helper prelude once: {down_sql}"
        );
        assert!(
            up_sql.find("CREATE SCHEMA IF NOT EXISTS djogi;").unwrap()
                < up_sql.find("-- add metric check").unwrap(),
            "Up migration prelude must appear before operations: {up_sql}"
        );
        assert!(
            down_sql.find("CREATE SCHEMA IF NOT EXISTS djogi;").unwrap()
                < down_sql.find("-- add metric check").unwrap(),
            "Down migration prelude must appear before operations: {down_sql}"
        );
        assert!(
            up_sql.contains("djogi.__djogi_numeric_array_is_rust_decimal_v1(\"metrics\")"),
            "Up migration should reference helper: {up_sql}"
        );
        assert!(
            !up_sql.contains("NOT EXISTS (SELECT 1 FROM unnest(\"metrics\")"),
            "Numeric helper CHECK should not use subquery style: {up_sql}"
        );
    }

    #[test]
    fn compose_up_text_omits_numeric_array_helper_when_unreferenced() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".into(),
                app: "billing".into(),
            },
            operations: Vec::new(),
            classification: Classification::Reversible,
        };
        let lowered = vec![OperationSql {
            label: "add integer col".into(),
            up: r#"ALTER TABLE "accounts" ADD COLUMN "score" integer CHECK ("score" >= 0);"#.into(),
            down: r#"ALTER TABLE "accounts" DROP COLUMN "score";"#.into(),
            lossy: None,
        }];
        let version = "V20260518__no_numeric_array_helper";
        let up_sql = compose_up_text(version, &delta, &lowered);
        let down_sql = compose_down_text(version, &delta, &lowered);

        assert!(
            !up_sql.contains("CREATE SCHEMA IF NOT EXISTS djogi;"),
            "Up migration without helper references must not include prelude: {up_sql}"
        );
        assert!(
            !down_sql.contains("CREATE SCHEMA IF NOT EXISTS djogi;"),
            "Down migration without helper references must not include prelude: {down_sql}"
        );
    }

    #[test]
    fn compose_numeric_array_helper_prelude_uses_only_valid_schema_qualified_identifiers() {
        let expected_identifiers = [
            "numeric", "bool", "bool_and", "scale", "abs", "power", "numeric", "unnest",
        ];

        assert_helper_prelude_uses_input_array_argument(
            NUMERIC_ARRAY_HELPER_PRELUDE,
            "__djogi_numeric_array_is_rust_decimal_v1",
            "numeric",
        );
        assert_helper_prelude_uses_pg_catalog_bool_return_type(NUMERIC_ARRAY_HELPER_PRELUDE);
        let found_identifiers = pg_catalog_identifiers(NUMERIC_ARRAY_HELPER_PRELUDE);
        for id in &found_identifiers {
            assert!(
                expected_identifiers.contains(&id.as_str()),
                "Unexpected schema qualification in helper prelude: pg_catalog.{id}"
            );
        }
        assert!(
            !found_identifiers.iter().any(|id| *id == "coalesce"),
            "COALESCE is conditional-expression syntax and must not be schema-qualified: {NUMERIC_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            NUMERIC_ARRAY_HELPER_PRELUDE.contains("SELECT COALESCE("),
            "Helper body should use PostgreSQL conditional-expression syntax: COALESCE"
        );
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    // `tokio_postgres::connect` is used in this substrate integration test to
    // validate SQL execution against a live database where configured.
    async fn compose_numeric_array_helper_prelude_applies_in_postgres_when_database_url_present() {
        use std::env;
        let database_url = match env::var("DATABASE_URL") {
            Ok(database_url) if !database_url.is_empty() => database_url,
            _ => return,
        };

        let (mut client, connection) =
            tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
                .await
                .unwrap();
        let connection = tokio::spawn(async move {
            if let Err(e) = connection.await {
                panic!("Postgres connection task failed: {e}");
            }
        });

        let tx = client.transaction().await.unwrap();
        tx.batch_execute(NUMERIC_ARRAY_HELPER_PRELUDE)
            .await
            .unwrap();
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_numeric_array_is_rust_decimal_v1(ARRAY[1::numeric, 2::numeric]::numeric[])",
                &[],
            )
            .await
            .unwrap();
        assert!(row.get::<_, bool>(0));
        tx.rollback().await.unwrap();
        // Drop the client before awaiting the connection task; while the
        // client is alive the connection task keeps waiting for more
        // requests, causing `connection.await` to deadlock.
        drop(client);

        connection.await.unwrap();
    }

    // ── Temporal array helper tests ──────────────────────────────────────────

    #[test]
    fn compose_up_down_prepends_date_array_helper_when_check_is_referenced() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".into(),
                app: "scheduling".into(),
            },
            operations: Vec::new(),
            classification: Classification::Reversible,
        };
        let lowered = vec![OperationSql {
            label: "add blackout dates check".into(),
            up: r#"ALTER TABLE "calendars" ADD CONSTRAINT "blackout_dates_check" CHECK (djogi.__djogi_date_array_is_finite_v1("blackout_dates"));"#
                .into(),
            down: r#"ALTER TABLE "calendars" DROP CONSTRAINT "blackout_dates_check";"#
                .into(),
            lossy: None,
        }];
        let version = "V20260518__date_array_helper";
        let up_sql = compose_up_text(version, &delta, &lowered);
        let down_sql = compose_down_text(version, &delta, &lowered);

        assert!(
            up_sql.contains(DATE_ARRAY_HELPER_PRELUDE),
            "Up migration must include date-array helper prelude: {up_sql}"
        );
        assert!(
            down_sql.contains(DATE_ARRAY_HELPER_PRELUDE),
            "Down migration must include date-array helper prelude: {down_sql}"
        );
        assert!(
            up_sql.find("CREATE SCHEMA IF NOT EXISTS djogi;").unwrap()
                < up_sql.find("-- add blackout dates check").unwrap(),
            "Date-array prelude must appear before operations in up migration: {up_sql}"
        );
        assert!(
            !up_sql.contains("djogi.__djogi_numeric_array_is_rust_decimal_v1("),
            "Date-array migration must not inject unneeded numeric helper: {up_sql}"
        );
        assert!(
            !up_sql.contains("djogi.__djogi_tstz_array_is_finite_v1("),
            "Date-array migration must not inject unneeded tstz helper: {up_sql}"
        );
    }

    #[test]
    fn compose_up_down_prepends_tstz_array_helper_when_check_is_referenced() {
        let delta = SchemaDelta {
            bucket: BucketKey {
                database: "main".into(),
                app: "events".into(),
            },
            operations: Vec::new(),
            classification: Classification::Reversible,
        };
        let lowered = vec![OperationSql {
            label: "add scheduled slots check".into(),
            up: r#"ALTER TABLE "sessions" ADD CONSTRAINT "slots_check" CHECK (djogi.__djogi_tstz_array_is_finite_v1("slots"));"#
                .into(),
            down: r#"ALTER TABLE "sessions" DROP CONSTRAINT "slots_check";"#.into(),
            lossy: None,
        }];
        let version = "V20260518__tstz_array_helper";
        let up_sql = compose_up_text(version, &delta, &lowered);
        let down_sql = compose_down_text(version, &delta, &lowered);

        assert!(
            up_sql.contains(TSTZ_ARRAY_HELPER_PRELUDE),
            "Up migration must include tstz-array helper prelude: {up_sql}"
        );
        assert!(
            down_sql.contains(TSTZ_ARRAY_HELPER_PRELUDE),
            "Down migration must include tstz-array helper prelude: {down_sql}"
        );
        assert!(
            up_sql.find("CREATE SCHEMA IF NOT EXISTS djogi;").unwrap()
                < up_sql.find("-- add scheduled slots check").unwrap(),
            "Tstz-array prelude must appear before operations in up migration: {up_sql}"
        );
        assert!(
            !up_sql.contains("djogi.__djogi_numeric_array_is_rust_decimal_v1("),
            "Tstz-array migration must not inject unneeded numeric helper: {up_sql}"
        );
        assert!(
            !up_sql.contains("djogi.__djogi_date_array_is_finite_v1("),
            "Tstz-array migration must not inject unneeded date-array helper: {up_sql}"
        );
    }

    #[test]
    fn compose_date_array_helper_prelude_uses_only_valid_schema_qualified_identifiers() {
        // date[], bool, isfinite, bool_and, date, unnest — all legitimate
        // pg_catalog-qualified identifiers. COALESCE is a conditional-expression
        // keyword and must NOT be schema-qualified.
        let expected_identifiers = ["date", "bool", "isfinite", "bool_and", "unnest"];

        assert_helper_prelude_uses_input_array_argument(
            DATE_ARRAY_HELPER_PRELUDE,
            "__djogi_date_array_is_finite_v1",
            "date",
        );
        assert_helper_prelude_uses_pg_catalog_bool_return_type(DATE_ARRAY_HELPER_PRELUDE);
        let found_identifiers = pg_catalog_identifiers(DATE_ARRAY_HELPER_PRELUDE);
        for id in &found_identifiers {
            assert!(
                expected_identifiers.contains(&id.as_str()),
                "Unexpected schema qualification in date-array helper: pg_catalog.{id}"
            );
        }
        assert!(
            !found_identifiers.iter().any(|id| *id == "coalesce"),
            "COALESCE must not be schema-qualified: {DATE_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            DATE_ARRAY_HELPER_PRELUDE.contains("SELECT COALESCE("),
            "Date-array helper body should use unqualified COALESCE: {DATE_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            DATE_ARRAY_HELPER_PRELUDE.contains("pg_catalog.isfinite(value)"),
            "Date-array helper must guard against both ±infinity via isfinite: \
             {DATE_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            DATE_ARRAY_HELPER_PRELUDE.contains("'9999-12-31'::pg_catalog.date"),
            "Date-array helper must cap at time::Date MAX (9999-12-31): {DATE_ARRAY_HELPER_PRELUDE}"
        );
    }

    #[test]
    fn compose_tstz_array_helper_prelude_uses_only_valid_schema_qualified_identifiers() {
        let expected_identifiers = ["timestamptz", "bool", "isfinite", "bool_and", "unnest"];

        assert_helper_prelude_uses_input_array_argument(
            TSTZ_ARRAY_HELPER_PRELUDE,
            "__djogi_tstz_array_is_finite_v1",
            "timestamptz",
        );
        assert_helper_prelude_uses_pg_catalog_bool_return_type(TSTZ_ARRAY_HELPER_PRELUDE);
        let found_identifiers = pg_catalog_identifiers(TSTZ_ARRAY_HELPER_PRELUDE);
        for id in &found_identifiers {
            assert!(
                expected_identifiers.contains(&id.as_str()),
                "Unexpected schema qualification in tstz-array helper: pg_catalog.{id}"
            );
        }
        assert!(
            !found_identifiers.iter().any(|id| *id == "coalesce"),
            "COALESCE must not be schema-qualified: {TSTZ_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            TSTZ_ARRAY_HELPER_PRELUDE.contains("SELECT COALESCE("),
            "Tstz-array helper body should use unqualified COALESCE: {TSTZ_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            TSTZ_ARRAY_HELPER_PRELUDE.contains("pg_catalog.isfinite(value)"),
            "Tstz-array helper must guard against both ±infinity via isfinite: \
             {TSTZ_ARRAY_HELPER_PRELUDE}"
        );
        assert!(
            TSTZ_ARRAY_HELPER_PRELUDE
                .contains("'9999-12-31 23:59:59.999999+00'::pg_catalog.timestamptz"),
            "Tstz-array helper must cap at time::OffsetDateTime MAX (UTC): {TSTZ_ARRAY_HELPER_PRELUDE}"
        );
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    // `tokio_postgres::connect` is used in this substrate integration test to
    // validate SQL execution against a live database where configured.
    async fn compose_date_array_helper_prelude_applies_in_postgres_when_database_url_present() {
        use std::env;
        let database_url = match env::var("DATABASE_URL") {
            Ok(database_url) if !database_url.is_empty() => database_url,
            _ => return,
        };

        let (mut client, connection) =
            tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
                .await
                .unwrap();
        let connection = tokio::spawn(async move {
            if let Err(e) = connection.await {
                panic!("Postgres connection task failed: {e}");
            }
        });

        let tx = client.transaction().await.unwrap();
        tx.batch_execute(DATE_ARRAY_HELPER_PRELUDE).await.unwrap();
        // Finite dates: helper returns true.
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_date_array_is_finite_v1(ARRAY['2026-05-18'::date, '2000-01-01'::date]::date[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            row.get::<_, bool>(0),
            "finite date array must pass the helper check"
        );
        // Positive infinity: helper returns false (isfinite fails).
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_date_array_is_finite_v1(ARRAY['infinity'::date]::date[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            !row.get::<_, bool>(0),
            "date array containing +infinity must fail the helper check"
        );
        // Negative infinity: helper returns false (isfinite fails).
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_date_array_is_finite_v1(ARRAY['-infinity'::date]::date[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            !row.get::<_, bool>(0),
            "date array containing -infinity must fail the helper check"
        );
        // Empty array: helper returns true (COALESCE(NULL, true)).
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_date_array_is_finite_v1(ARRAY[]::date[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            row.get::<_, bool>(0),
            "empty date array must pass the helper check"
        );
        tx.rollback().await.unwrap();
        // Drop the client before awaiting the connection task; while the
        // client is alive the connection task keeps waiting for more
        // requests, causing `connection.await` to deadlock.
        drop(client);

        connection.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    // `tokio_postgres::connect` is used in this substrate integration test to
    // validate SQL execution against a live database where configured.
    async fn compose_tstz_array_helper_prelude_applies_in_postgres_when_database_url_present() {
        use std::env;
        let database_url = match env::var("DATABASE_URL") {
            Ok(database_url) if !database_url.is_empty() => database_url,
            _ => return,
        };

        let (mut client, connection) =
            tokio_postgres::connect(&database_url, tokio_postgres::NoTls)
                .await
                .unwrap();
        let connection = tokio::spawn(async move {
            if let Err(e) = connection.await {
                panic!("Postgres connection task failed: {e}");
            }
        });

        let tx = client.transaction().await.unwrap();
        tx.batch_execute(TSTZ_ARRAY_HELPER_PRELUDE).await.unwrap();
        // Finite timestamptz values: helper returns true.
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_tstz_array_is_finite_v1(ARRAY['2026-05-18 00:00:00+00'::timestamptz, '2000-01-01 12:00:00+00'::timestamptz]::timestamptz[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            row.get::<_, bool>(0),
            "finite timestamptz array must pass the helper check"
        );
        // Positive infinity: helper returns false (isfinite fails).
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_tstz_array_is_finite_v1(ARRAY['infinity'::timestamptz]::timestamptz[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            !row.get::<_, bool>(0),
            "timestamptz array containing +infinity must fail the helper check"
        );
        // Negative infinity: helper returns false (isfinite fails).
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_tstz_array_is_finite_v1(ARRAY['-infinity'::timestamptz]::timestamptz[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            !row.get::<_, bool>(0),
            "timestamptz array containing -infinity must fail the helper check"
        );
        // Empty array: helper returns true (COALESCE(NULL, true)).
        let row = tx
            .query_one(
                "SELECT djogi.__djogi_tstz_array_is_finite_v1(ARRAY[]::timestamptz[])",
                &[],
            )
            .await
            .unwrap();
        assert!(
            row.get::<_, bool>(0),
            "empty timestamptz array must pass the helper check"
        );
        tx.rollback().await.unwrap();
        // Drop the client before awaiting the connection task; while the
        // client is alive the connection task keeps waiting for more
        // requests, causing `connection.await` to deadlock.
        drop(client);

        connection.await.unwrap();
    }

    fn assert_helper_prelude_uses_input_array_argument(
        prelude: &str,
        function_name: &str,
        pg_type: &str,
    ) {
        let expected_signature = format!(
            "CREATE OR REPLACE FUNCTION djogi.{function_name}(input_array pg_catalog.{pg_type}[])"
        );
        assert!(
            prelude.contains(&expected_signature),
            "Helper prelude must use a non-keyword input_array argument in its signature: {prelude}"
        );
        assert!(
            prelude.contains("FROM pg_catalog.unnest(input_array) AS value(value);"),
            "Helper body must reference the renamed input_array argument: {prelude}"
        );
        let rejected_signature = format!("{function_name}(values pg_catalog.{pg_type}[])");
        assert!(
            !prelude.contains(&rejected_signature),
            "Helper prelude must not use PostgreSQL keyword `values` as an argument: {prelude}"
        );
        assert!(
            !prelude.contains("pg_catalog.unnest(values)"),
            "Helper body must not reference the rejected `values` argument: {prelude}"
        );
    }

    #[test]
    fn compose_up_header_contains_apply_via_cli_warning() {
        let version = "V20260425010203__add_users";
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "myapp".to_string(),
        };
        let delta = SchemaDelta {
            bucket,
            operations: vec![],
            classification: Classification::Additive,
        };
        let lowered: Vec<OperationSql> = vec![];
        let text = compose_up_text(version, &delta, &lowered);

        assert!(
            text.contains("Apply via `djogi migrations apply`"),
            "up header must contain apply-via-CLI warning: {text}"
        );
        assert!(
            text.contains("not psql"),
            "up header must name psql as the bypass path: {text}"
        );
        assert!(
            text.contains("ledger recording"),
            "up header must mention ledger: {text}"
        );
        assert!(
            text.contains("-- DO NOT EDIT"),
            "up header must still contain DO NOT EDIT line: {text}"
        );
        // Warning must appear BEFORE DO NOT EDIT
        let apply_pos = text.find("Apply via `djogi migrations apply`").unwrap();
        let dont_edit_pos = text.find("-- DO NOT EDIT").unwrap();
        assert!(
            apply_pos < dont_edit_pos,
            "apply warning ({apply_pos}) must precede DO NOT EDIT ({dont_edit_pos})"
        );
    }

    #[test]
    fn compose_down_header_contains_apply_via_cli_warning() {
        let version = "V20260425010203__add_users";
        let bucket = BucketKey {
            database: "main".to_string(),
            app: "myapp".to_string(),
        };
        let delta = SchemaDelta {
            bucket,
            operations: vec![],
            classification: Classification::Additive,
        };
        let lowered: Vec<OperationSql> = vec![];
        let text = compose_down_text(version, &delta, &lowered);

        assert!(
            text.contains("Apply via `djogi migrations apply`"),
            "down header must contain apply-via-CLI warning: {text}"
        );
        assert!(
            text.contains("-- DO NOT EDIT"),
            "down header must still contain DO NOT EDIT line: {text}"
        );
    }

    fn assert_helper_prelude_uses_pg_catalog_bool_return_type(prelude: &str) {
        assert!(
            prelude.contains("\nRETURNS pg_catalog.bool\n"),
            "Helper prelude must use PostgreSQL's schema-qualified bool type: {prelude}"
        );
        assert!(
            !prelude.contains("RETURNS pg_catalog.boolean"),
            "Helper prelude must not use PostgreSQL's unqualified-only boolean alias with a schema: {prelude}"
        );
    }

    fn pg_catalog_identifiers(sql: &str) -> Vec<String> {
        const PREFIX: &str = "pg_catalog.";
        let mut ids = Vec::new();
        let mut cursor = 0usize;

        while let Some(offset) = sql[cursor..].find(PREFIX) {
            let ident_start = cursor + offset + PREFIX.len();
            let rest = &sql[ident_start..];
            let mut len = 0usize;
            for b in rest.bytes() {
                if len == 0 {
                    if !(b.is_ascii_alphabetic() || b == b'_') {
                        break;
                    }
                } else if !(b.is_ascii_alphanumeric() || b == b'_') {
                    break;
                }
                len += 1;
            }
            if len > 0 {
                ids.push(rest[..len].to_string());
            }
            cursor = ident_start + len.max(1);
        }

        ids
    }

    // ── REQ-370-16 linkage-aware drop guard tests ────────────────────

    #[test]
    fn linkage_drop_guard_fires_with_allow_destructive() {
        // Snapshot bucket (main, "billing") has one table. Projection has zero
        // models for that bucket. App is NOT tombstoned, NOT renamed.
        // allow_destructive = true. Guard MUST fire.
        let work = temp_workspace("linkage_fire");
        let guard = lock_for(&work);

        let billing_bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            billing_bucket.clone(),
            snapshot_with_widgets(&billing_bucket),
        );

        // Zero projected models — model crate not linked
        let models: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();

        let apps = vec![AppLifecycle {
            label: "billing".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "drop_billing",
            allow_destructive: true,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let err =
            compose(req).expect_err("linkage guard must refuse even with --allow-destructive");
        assert!(
            matches!(err, ComposeError::LinkageDropWithoutModels { ref app_label, .. } if app_label == "billing"),
            "expected LinkageDropWithoutModels for billing, got: {err}"
        );
        cleanup_workspace(&work);
    }

    #[test]
    fn linkage_drop_guard_allows_tombstoned_app_removal() {
        // Same shape as above but tombstone = true. Guard must NOT fire.
        let work = temp_workspace("linkage_tombstone");
        let guard = lock_for(&work);

        let billing_bucket = BucketKey {
            database: "main".to_string(),
            app: "billing".to_string(),
        };
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            billing_bucket.clone(),
            snapshot_with_widgets(&billing_bucket),
        );

        // Zero projected models
        let models: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();

        let apps = vec![AppLifecycle {
            label: "billing".to_string(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: true, // ← intentional removal
        }];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "drop_billing_tombstone",
            allow_destructive: true,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        match compose(req) {
            Err(ComposeError::LinkageDropWithoutModels { .. }) => {
                panic!("tombstoned removal must not trip the linkage guard")
            }
            _ => {
                // Any other outcome acceptable — tombstone path owns this.
                // The D011 gate may fire first if allow_destructive were false,
                // or the differ may produce results. We only assert the linkage
                // guard does NOT fire.
            }
        }
        cleanup_workspace(&work);
    }

    #[test]
    fn linkage_drop_guard_does_not_fire_for_renamed_app() {
        // Renamed app must NOT fire. Snapshot under OLD key,
        // projection under NEW key, remap moves snapshot to NEW where models exist.
        let work = temp_workspace("linkage_rename");
        let guard = lock_for(&work);

        let old_bucket = BucketKey {
            database: "main".to_string(),
            app: "accounts".to_string(),
        };
        let new_bucket = BucketKey {
            database: "main".to_string(),
            app: "ledger".to_string(),
        };

        // Snapshot has tables under OLD key (before rename)
        let mut snapshots = BTreeMap::new();
        snapshots.insert(old_bucket.clone(), snapshot_with_widgets(&old_bucket));

        // Models under NEW key — after remap, snapshots_for_diff moves the
        // old_bucket snapshot to new_bucket. Both sides have models → no guard fire.
        let mut models = BTreeMap::new();
        models.insert(new_bucket.clone(), snapshot_with_widgets(&new_bucket));

        let apps = vec![AppLifecycle {
            label: "ledger".to_string(),
            database: "main".to_string(),
            renamed_from: Some("accounts".to_string()),
            tombstone: false,
        }];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "rename_ledger",
            allow_destructive: true,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        match compose(req) {
            Err(ComposeError::LinkageDropWithoutModels { .. }) => {
                panic!("renamed app must not trip the linkage guard")
            }
            _ => {
                // Any other outcome acceptable — rename should proceed
                // without triggering the linkage guard.
            }
        }
        cleanup_workspace(&work);
    }

    #[test]
    fn linkage_drop_guard_fires_for_emptied_global_bucket() {
        // Synthetic global bucket guarded uniformly when an app
        // descriptor exists for it.
        let work = temp_workspace("linkage_global");
        let guard = lock_for(&work);

        let global_bucket = BucketKey {
            database: "main".to_string(),
            app: String::new(),
        };
        let mut snapshots = BTreeMap::new();
        snapshots.insert(global_bucket.clone(), snapshot_with_widgets(&global_bucket));

        // Global bucket now empty — models vanished
        let models: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();

        // App descriptor exists for global bucket (un-#[model(app=)] models)
        let apps = vec![AppLifecycle {
            label: String::new(),
            database: "main".to_string(),
            renamed_from: None,
            tombstone: false,
        }];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "drop_global",
            allow_destructive: true,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let err = compose(req).expect_err("global bucket guard must fire");
        assert!(
            matches!(err, ComposeError::LinkageDropWithoutModels { ref app_label, .. } if app_label.is_empty()),
            "expected LinkageDropWithoutModels for global bucket, got: {err}"
        );
        cleanup_workspace(&work);
    }

    #[test]
    fn linkage_drop_guard_fires_when_app_absent_from_registry() {
        // Spec compliance: guard keys on zero projected models per bucket,
        // NOT on app presence in req.apps. If an app was deregistered
        // without tombstone and its model crate is unlinked, the guard
        // must fire — the tombstone channel is the only intentional removal.
        let work = temp_workspace("linkage_absent");
        let guard = lock_for(&work);

        let orphan_bucket = BucketKey {
            database: "main".to_string(),
            app: "orphan".to_string(),
        };
        let mut snapshots = BTreeMap::new();
        snapshots.insert(orphan_bucket.clone(), snapshot_with_widgets(&orphan_bucket));

        // Zero projected models — model crate not linked
        let models: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();

        // Orphan app NOT in req.apps at all
        let apps: Vec<AppLifecycle> = vec![];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "drop_orphan",
            allow_destructive: true,
            force_overwrite: false,
            now: at(2026, 4, 25, 1, 2, 3),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let err = compose(req).expect_err("guard must fire even when app absent from req.apps");
        assert!(
            matches!(err, ComposeError::LinkageDropWithoutModels { ref app_label, .. } if app_label == "orphan"),
            "expected LinkageDropWithoutModels for orphan, got: {err}"
        );
        cleanup_workspace(&work);
    }

    // ── Cross-bucket FK ordering tests (#398) ──────────────────────

    /// Build a `ForeignKeySchema` for a column referencing the
    /// `users` table. Used by the two-bucket FK fixture.
    fn fk_to_users() -> crate::migrate::schema::ForeignKeySchema {
        use crate::migrate::schema::{ForeignKeySchema, OnDeleteSchema};
        ForeignKeySchema {
            deferrable: false,
            initially_deferred: false,
            on_delete: OnDeleteSchema::Restrict,
            ref_column: "id".to_string(),
            ref_table: "users".to_string(),
        }
    }

    /// Build a column that carries an FK to the `users` table.
    fn col_with_fk_to_users() -> ColumnSchema {
        ColumnSchema {
            name: "user_id".to_string(),
            sql_type: "BIGINT".to_string(),
            nullable: false,
            foreign_key: Some(fk_to_users()),
            ..col("user_id", "BIGINT", false)
        }
    }

    /// Build a table `event_log` with an FK column to `users`.
    fn table_event_log_with_fk(bucket: &BucketKey) -> TableSchema {
        TableSchema {
            app: if bucket.app.is_empty() {
                None
            } else {
                Some(bucket.app.clone())
            },
            columns: vec![id_column_heerid_desc(), col_with_fk_to_users()],
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
            table: "event_log".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    /// Build a simple `users` table (PK only).
    fn table_users(bucket: &BucketKey) -> TableSchema {
        TableSchema {
            app: if bucket.app.is_empty() {
                None
            } else {
                Some(bucket.app.clone())
            },
            columns: vec![id_column_heerid_desc()],
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
            table: "users".to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    /// Build an `AppliedSchema` with a single enum and no models.
    fn snapshot_with_enum(bucket: &BucketKey, name: &str, variants: &[&str]) -> AppliedSchema {
        let mut s = empty_snapshot(bucket);
        s.enums.insert(
            name.to_string(),
            EnumSchema {
                name: name.to_string(),
                variants: variants.iter().map(|s| s.to_string()).collect(),
            },
        );
        s
    }

    /// Build a simple table with an enum-typed column (e.g. `mood`).
    fn table_with_enum_col(
        bucket: &BucketKey,
        table: &str,
        col_name: &str,
        enum_type: &str,
    ) -> TableSchema {
        TableSchema {
            app: if bucket.app.is_empty() {
                None
            } else {
                Some(bucket.app.clone())
            },
            columns: vec![
                id_column_heerid_desc(),
                ColumnSchema {
                    name: col_name.to_string(),
                    sql_type: enum_type.to_string(),
                    nullable: true,
                    foreign_key: None,
                    ..col(col_name, enum_type, true)
                },
            ],
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
            table: table.to_string(),
            table_comment: None,
            storage_params: None,
            tablespace: None,
            tenant_key: None,
        }
    }

    /// Read a PendingPlan from the composed report for a given bucket.
    fn read_written_pending(
        work: &Path,
        report: &ComposeReport,
        database: &str,
        app: &str,
    ) -> PendingPlan {
        let bucket = BucketKey {
            database: database.to_string(),
            app: app.to_string(),
        };
        let cb = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == bucket)
            .unwrap_or_else(|| panic!("composed bucket for {database}/{app}"));
        let bytes = crate::migrate::read_workspace_file(work, &cb.pending_json_path)
            .unwrap_or_else(|_| panic!("read pending for {database}/{app}"));
        parse_pending_bytes(&bytes, Some(cb.pending_json_path.clone()))
            .unwrap_or_else(|e| panic!("parse pending for {database}/{app}: {e}"))
    }

    /// system.event_log carries an FK to users.users (different bucket,
    /// same database) — compose must record system -> depends_on ["users"]
    /// and leave users' depends_on empty.
    #[test]
    fn cross_bucket_fk_records_depends_on() {
        let work = temp_workspace("two-bucket-fk");
        let guard = lock_for(&work);

        let users_bucket = BucketKey {
            database: "main".into(),
            app: "users".into(),
        };
        let system_bucket = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };

        // Models: both buckets have tables
        let mut models = BTreeMap::new();
        {
            let mut users_schema = empty_snapshot(&users_bucket);
            users_schema
                .models
                .insert("users".to_string(), table_users(&users_bucket));
            models.insert(users_bucket.clone(), users_schema);
        }
        {
            let mut system_schema = empty_snapshot(&system_bucket);
            system_schema.models.insert(
                "event_log".to_string(),
                table_event_log_with_fk(&system_bucket),
            );
            models.insert(system_bucket.clone(), system_schema);
        }

        // Snapshots: empty (fresh compose) — clear models so differ sees new tables
        let mut snapshots = BTreeMap::new();
        snapshots.insert(users_bucket.clone(), empty_snapshot(&users_bucket));
        snapshots.insert(system_bucket.clone(), empty_snapshot(&system_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "users".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "system".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "cross-bucket-fk",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");
        let system = read_written_pending(&work, &report, "main", "system");
        let users = read_written_pending(&work, &report, "main", "users");
        assert_eq!(
            system.depends_on,
            vec!["users".to_string()],
            "system should depend on users (cross-bucket FK)"
        );
        assert!(
            users.depends_on.is_empty(),
            "users should have no dependencies"
        );
        cleanup_workspace(&work);
    }

    // ── Unit tests for helper functions ────────────────────────────

    #[test]
    fn fk_target_tables_add_table_with_inline_fk() {
        use crate::migrate::diff::SchemaOperation;
        let bucket = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };
        let op = SchemaOperation::AddTable(table_event_log_with_fk(&bucket));
        let targets = fk_target_tables(&op);
        assert_eq!(targets, vec!["users".to_string()]);
    }

    #[test]
    fn fk_target_tables_add_foreign_key() {
        use crate::migrate::diff::SchemaOperation;
        let op = SchemaOperation::AddForeignKey {
            table: "orders".to_string(),
            column: "user_id".to_string(),
            fk: fk_to_users(),
        };
        let targets = fk_target_tables(&op);
        assert_eq!(targets, vec!["users".to_string()]);
    }

    #[test]
    fn fk_target_tables_add_column_with_fk() {
        use crate::migrate::diff::SchemaOperation;
        let col = col_with_fk_to_users();
        let op = SchemaOperation::AddColumn {
            table: "orders".to_string(),
            column: col,
        };
        let targets = fk_target_tables(&op);
        assert_eq!(targets, vec!["users".to_string()]);
    }

    #[test]
    fn fk_target_tables_drop_table_returns_empty() {
        use crate::migrate::diff::SchemaOperation;
        let op = SchemaOperation::DropTable("widgets".to_string());
        assert!(fk_target_tables(&op).is_empty());
    }

    #[test]
    fn order_buckets_acyclic_two_bucket_order() {
        use std::collections::{BTreeMap, BTreeSet};

        let _users_bucket = BucketKey {
            database: "main".into(),
            app: "users".into(),
        };
        let system_bucket = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };

        // system depends on users
        let mut deps = BTreeMap::new();
        let mut system_deps = BTreeSet::new();
        system_deps.insert("users".to_string());
        deps.insert(system_bucket, system_deps);

        let buckets: BTreeSet<String> = BTreeSet::from_iter(vec!["users".into(), "system".into()]);
        let order = order_buckets("main", &buckets, &deps).expect("no cycle");
        assert_eq!(order, vec!["users", "system"]);
    }

    #[test]
    fn order_buckets_dependency_on_missing_bucket_is_ignored() {
        use std::collections::{BTreeMap, BTreeSet};

        let system_bucket = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };

        // system depends on "billing", but billing is not in the buckets set
        let mut deps = BTreeMap::new();
        let mut system_deps = BTreeSet::new();
        system_deps.insert("billing".to_string());
        deps.insert(system_bucket, system_deps);

        let buckets: BTreeSet<String> = BTreeSet::from_iter(vec!["system".into()]);
        let order = order_buckets("main", &buckets, &deps).expect("no cycle");
        assert_eq!(order, vec!["system"]);
    }

    #[test]
    fn order_buckets_cycle_returns_error() {
        use std::collections::{BTreeMap, BTreeSet};

        let a_bucket = BucketKey {
            database: "main".into(),
            app: "a".into(),
        };
        let b_bucket = BucketKey {
            database: "main".into(),
            app: "b".into(),
        };

        // a -> b and b -> a (cycle)
        let mut deps = BTreeMap::new();
        let mut a_deps = BTreeSet::new();
        a_deps.insert("b".to_string());
        deps.insert(a_bucket, a_deps);

        let mut b_deps = BTreeSet::new();
        b_deps.insert("a".to_string());
        deps.insert(b_bucket, b_deps);

        let buckets: BTreeSet<String> = BTreeSet::from_iter(vec!["a".into(), "b".into()]);
        let err = order_buckets("main", &buckets, &deps).expect_err("cycle detected");
        match err {
            ComposeError::CrossBucketForeignKeyCycle { database, chain } => {
                assert_eq!(database, "main");
                assert!(chain.contains(&"a".to_string()));
                assert!(chain.contains(&"b".to_string()));
            }
            _ => panic!("expected CrossBucketForeignKeyCycle, got: {err:?}"),
        }
    }

    #[test]
    fn cross_bucket_cycle_compose_refuses() {
        let work = temp_workspace("cross-bucket-cycle");
        let guard = lock_for(&work);

        let a_bucket = BucketKey {
            database: "main".into(),
            app: "a".into(),
        };
        let b_bucket = BucketKey {
            database: "main".into(),
            app: "b".into(),
        };

        // Build FK from a's table to b's table and vice versa
        fn fk_to_table(table_name: &str) -> crate::migrate::schema::ForeignKeySchema {
            use crate::migrate::schema::{ForeignKeySchema, OnDeleteSchema};
            ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: table_name.to_string(),
            }
        }

        fn col_fk(name: &str, target: &str) -> ColumnSchema {
            ColumnSchema {
                name: format!("{name}_id"),
                sql_type: "BIGINT".to_string(),
                nullable: false,
                foreign_key: Some(fk_to_table(target)),
                ..col(&format!("{name}_id"), "BIGINT", false)
            }
        }

        fn simple_table(
            name: &str,
            fk_col_name: &str,
            fk_target: &str,
            bucket: &BucketKey,
        ) -> TableSchema {
            TableSchema {
                app: if bucket.app.is_empty() {
                    None
                } else {
                    Some(bucket.app.clone())
                },
                columns: vec![id_column_heerid_desc(), col_fk(fk_col_name, fk_target)],
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
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        // a has table_a with FK to b's table_b
        let mut models = BTreeMap::new();
        {
            let mut a_schema = empty_snapshot(&a_bucket);
            a_schema.models.insert(
                "table_a".to_string(),
                simple_table("table_a", "b", "table_b", &a_bucket),
            );
            models.insert(a_bucket.clone(), a_schema);
        }
        {
            let mut b_schema = empty_snapshot(&b_bucket);
            b_schema.models.insert(
                "table_b".to_string(),
                simple_table("table_b", "a", "table_a", &b_bucket),
            );
            models.insert(b_bucket.clone(), b_schema);
        }

        // Empty snapshots so differ sees new tables
        let mut snapshots = BTreeMap::new();
        snapshots.insert(a_bucket.clone(), empty_snapshot(&a_bucket));
        snapshots.insert(b_bucket.clone(), empty_snapshot(&b_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "a".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "b".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "cycle-test",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let err = compose(req).expect_err("cycle must be refused");
        match err {
            ComposeError::CrossBucketForeignKeyCycle { database, chain } => {
                assert_eq!(database, "main");
                assert!(chain.contains(&"a".to_string()));
                assert!(chain.contains(&"b".to_string()));
            }
            _ => panic!("expected CrossBucketForeignKeyCycle, got: {err:?}"),
        }
        cleanup_workspace(&work);
    }

    // ── Cross-database FK filtering ────────────────────────────────

    /// Two buckets in different databases: analytics/events has an FK to
    /// a table in the main database. The depends_on list must be empty
    /// because cross-database references are not ordering edges.
    #[test]
    fn cross_bucket_fk_different_databases_is_filtered() {
        let work = temp_workspace("cross-db-fk-filter");
        let guard = lock_for(&work);

        let users_bucket = BucketKey {
            database: "main".into(),
            app: "users".into(),
        };
        let events_bucket = BucketKey {
            database: "analytics".into(),
            app: "events".into(),
        };

        // Build an FK that references the "users" table (which lives in main)
        fn fk_to_users_table() -> crate::migrate::schema::ForeignKeySchema {
            use crate::migrate::schema::{ForeignKeySchema, OnDeleteSchema};
            ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "users".to_string(),
            }
        }

        fn col_fk_user(_bucket: &BucketKey) -> ColumnSchema {
            ColumnSchema {
                name: "user_id".to_string(),
                sql_type: "BIGINT".to_string(),
                nullable: false,
                foreign_key: Some(fk_to_users_table()),
                ..col("user_id", "BIGINT", false)
            }
        }

        fn table_events(bucket: &BucketKey) -> TableSchema {
            TableSchema {
                app: if bucket.app.is_empty() {
                    None
                } else {
                    Some(bucket.app.clone())
                },
                columns: vec![id_column_heerid_desc(), col_fk_user(bucket)],
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
                table: "page_views".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        // Models: both buckets exist in their respective databases
        let mut models = BTreeMap::new();
        {
            let mut users_schema = empty_snapshot(&users_bucket);
            users_schema
                .models
                .insert("users".to_string(), table_users(&users_bucket));
            models.insert(users_bucket.clone(), users_schema);
        }
        {
            let mut events_schema = empty_snapshot(&events_bucket);
            events_schema
                .models
                .insert("page_views".to_string(), table_events(&events_bucket));
            models.insert(events_bucket.clone(), events_schema);
        }

        // Empty snapshots so differ sees new tables
        let mut snapshots = BTreeMap::new();
        snapshots.insert(users_bucket.clone(), empty_snapshot(&users_bucket));
        snapshots.insert(events_bucket.clone(), empty_snapshot(&events_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "users".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "events".into(),
                database: "analytics".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "cross-db-fk",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");
        let events = read_written_pending(&work, &report, "analytics", "events");
        assert!(
            events.depends_on.is_empty(),
            "events should have no depends_on (cross-database FK to main/users is filtered out)"
        );
        cleanup_workspace(&work);
    }

    // ── Pre-existing target exclusion ──────────────────────────────

    /// Single bucket with a table carrying an FK to "users", but "users"
    /// table is NOT in the models map (pre-existing on disk). The
    /// depends_on list must be empty because the target is not tracked
    /// in this compose run.
    #[test]
    fn cross_bucket_fk_target_not_in_projection_is_ignored() {
        let work = temp_workspace("fk-preexisting-target");
        let guard = lock_for(&work);

        let system_bucket = BucketKey {
            database: "main".into(),
            app: "system".into(),
        };

        // Build a table with FK to "users" (which does NOT exist in models)
        fn table_orders(bucket: &BucketKey) -> TableSchema {
            TableSchema {
                app: if bucket.app.is_empty() {
                    None
                } else {
                    Some(bucket.app.clone())
                },
                columns: vec![id_column_heerid_desc(), col_with_fk_to_users()],
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
                table: "orders".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        let mut models = BTreeMap::new();
        {
            let mut system_schema = empty_snapshot(&system_bucket);
            system_schema
                .models
                .insert("orders".to_string(), table_orders(&system_bucket));
            models.insert(system_bucket.clone(), system_schema);
        }

        // Empty snapshot so differ sees new table
        let mut snapshots = BTreeMap::new();
        snapshots.insert(system_bucket.clone(), empty_snapshot(&system_bucket));

        let apps: Vec<AppLifecycle> = vec![AppLifecycle {
            label: "system".into(),
            database: "main".into(),
            renamed_from: None,
            tombstone: false,
        }];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "fk-preexisting",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");
        let system = read_written_pending(&work, &report, "main", "system");
        assert!(
            system.depends_on.is_empty(),
            "system should have no depends_on (FK target 'users' not in projection)"
        );
        cleanup_workspace(&work);
    }

    // ── Within-bucket FK exclusion ─────────────────────────────────

    /// Single bucket with two tables, one carrying an FK to the other.
    /// The depends_on list must be empty because within-bucket FKs are
    /// handled by segment.rs, not cross-bucket ordering.
    #[test]
    fn cross_bucket_fk_within_same_bucket_is_excluded() {
        let work = temp_workspace("within-bucket-fk");
        let guard = lock_for(&work);

        let orders_bucket = BucketKey {
            database: "main".into(),
            app: "orders".into(),
        };

        // FK referencing the "items" table (same bucket)
        fn fk_to_items() -> crate::migrate::schema::ForeignKeySchema {
            use crate::migrate::schema::{ForeignKeySchema, OnDeleteSchema};
            ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "items".to_string(),
            }
        }

        fn col_item_id() -> ColumnSchema {
            ColumnSchema {
                name: "item_id".to_string(),
                sql_type: "BIGINT".to_string(),
                nullable: false,
                foreign_key: Some(fk_to_items()),
                ..col("item_id", "BIGINT", false)
            }
        }

        fn table_line_items(bucket: &BucketKey) -> TableSchema {
            TableSchema {
                app: if bucket.app.is_empty() {
                    None
                } else {
                    Some(bucket.app.clone())
                },
                columns: vec![id_column_heerid_desc(), col_item_id()],
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
                table: "line_items".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        fn table_items(bucket: &BucketKey) -> TableSchema {
            TableSchema {
                app: if bucket.app.is_empty() {
                    None
                } else {
                    Some(bucket.app.clone())
                },
                columns: vec![id_column_heerid_desc()],
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
                table: "items".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        // Both tables in the same bucket
        let mut models = BTreeMap::new();
        {
            let mut orders_schema = empty_snapshot(&orders_bucket);
            orders_schema
                .models
                .insert("line_items".to_string(), table_line_items(&orders_bucket));
            orders_schema
                .models
                .insert("items".to_string(), table_items(&orders_bucket));
            models.insert(orders_bucket.clone(), orders_schema);
        }

        // Empty snapshot so differ sees both new tables
        let mut snapshots = BTreeMap::new();
        snapshots.insert(orders_bucket.clone(), empty_snapshot(&orders_bucket));

        let apps: Vec<AppLifecycle> = vec![AppLifecycle {
            label: "orders".into(),
            database: "main".into(),
            renamed_from: None,
            tombstone: false,
        }];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "within-bucket-fk",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");
        let orders = read_written_pending(&work, &report, "main", "orders");
        assert!(
            orders.depends_on.is_empty(),
            "orders should have no depends_on (within-bucket FK is excluded)"
        );
        cleanup_workspace(&work);
    }

    // ── Enum reconciliation tests (GH #396, Stage 3) ──────────────

    /// Two buckets both reference same enum `mood`. Assert exactly one
    /// CREATE TYPE across both buckets; owner is alphabetically first
    /// (no FK edges); non-owner has depends_on edge to owner.
    #[test]
    fn shared_enum_creates_once_with_ownership_edge() {
        let work = temp_workspace("shared-enum-create");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        // Both buckets project the `mood` enum + a table that uses it.
        let mood = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        let mut models = BTreeMap::new();
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert("mood".to_string(), mood.clone());
            alpha_schema.models.insert(
                "posts".to_string(),
                table_with_enum_col(&alpha_bucket, "posts", "mood", "mood"),
            );
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        {
            let mut beta_schema = empty_snapshot(&beta_bucket);
            beta_schema.enums.insert("mood".to_string(), mood.clone());
            beta_schema.models.insert(
                "comments".to_string(),
                table_with_enum_col(&beta_bucket, "comments", "author_mood", "mood"),
            );
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Empty snapshots — fresh compose, no prior enums.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(alpha_bucket.clone(), empty_snapshot(&alpha_bucket));
        snapshots.insert(beta_bucket.clone(), empty_snapshot(&beta_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "shared-enum-create",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");

        // Read up SQL for each bucket.
        let alpha_up = crate::migrate::read_workspace_file_to_string(
            &work,
            &report
                .composed_buckets
                .iter()
                .find(|c| c.bucket == alpha_bucket)
                .unwrap()
                .up_sql_path,
        )
        .unwrap();
        let beta_up = crate::migrate::read_workspace_file_to_string(
            &work,
            &report
                .composed_buckets
                .iter()
                .find(|c| c.bucket == beta_bucket)
                .unwrap()
                .up_sql_path,
        )
        .unwrap();

        // Exactly one CREATE TYPE across both buckets.
        let alpha_creates = alpha_up.contains("CREATE TYPE");
        let beta_creates = beta_up.contains("CREATE TYPE");
        assert!(
            alpha_creates && !beta_creates,
            "alpha should own CREATE TYPE; beta should not. alpha_up:\n{}\nbeta_up:\n{}",
            alpha_up,
            beta_up
        );

        // Non-owner (beta) depends on owner (alpha).
        let beta_pending = read_written_pending(&work, &report, "main", "beta");
        assert_eq!(
            beta_pending.depends_on,
            vec!["alpha".to_string()],
            "beta should depend on alpha for enum ownership"
        );

        // REQ-396-6: non-owner beta had AddEnum removed by dedup, so its
        // down migration must NOT drop the shared type.
        let beta_down = crate::migrate::read_workspace_file_to_string(
            &work,
            &report
                .composed_buckets
                .iter()
                .find(|c| c.bucket == beta_bucket)
                .unwrap()
                .down_sql_path,
        )
        .unwrap();
        assert!(
            !beta_down.contains("DROP TYPE"),
            "non-owner beta down-SQL must not DROP the shared enum type; beta_down:\n{beta_down}"
        );
        cleanup_workspace(&work);
    }

    /// REQ-396-11: Two buckets that both add the same new enum variant
    /// must produce exactly one `ALTER TYPE ... ADD VALUE` across both
    /// buckets. The topo-first bucket (alpha) keeps it; the non-owner
    /// (beta) has it removed; beta gains a dependency edge on alpha.
    /// This test calls `reconcile_enum_ops_across_buckets` directly so
    /// it is Postgres-free and runs in pure unit-test mode.
    #[test]
    fn shared_enum_variant_creates_once() {
        use crate::migrate::diff::{self, SchemaOperation};

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        // Both buckets project the `mood` enum in their current models
        // (the enum already exists — snapshots also carry it).
        let mut models = BTreeMap::new();
        models.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad", "excited"]),
        );
        models.insert(
            beta_bucket.clone(),
            snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad", "excited"]),
        );

        // Both snapshots carry the base enum (no AddEnum this run).
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad"]),
        );
        snapshots.insert(
            beta_bucket.clone(),
            snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad"]),
        );

        // Build two deltas, each with an AddEnumVariant(mood, excited).
        // anchor = None means tail-append; fine for this dedup test.
        let add_variant_op = SchemaOperation::AddEnumVariant {
            enum_name: "mood".to_string(),
            variant: "excited".to_string(),
            anchor: None,
        };

        let mut deltas = vec![
            SchemaDelta {
                bucket: alpha_bucket.clone(),
                operations: vec![add_variant_op.clone()],
                classification: diff::classify_operations(std::slice::from_ref(&add_variant_op)),
            },
            SchemaDelta {
                bucket: beta_bucket.clone(),
                operations: vec![add_variant_op.clone()],
                classification: diff::classify_operations(std::slice::from_ref(&add_variant_op)),
            },
        ];

        // No FK deps.
        let fk_deps = BTreeMap::new();

        let edges = reconcile_enum_ops_across_buckets(&mut deltas, &models, &snapshots, &fk_deps)
            .expect("reconcile succeeds");

        // 1. Exactly one AddEnumVariant(mood, excited) survives across both deltas.
        let surviving_count: usize = deltas
            .iter()
            .flat_map(|d| d.operations.iter())
            .filter(|op| {
                matches!(
                    op,
                    SchemaOperation::AddEnumVariant { enum_name, variant, .. }
                        if enum_name == "mood" && variant == "excited"
                )
            })
            .count();
        assert_eq!(
            surviving_count, 1,
            "exactly one AddEnumVariant(mood, excited) must survive; got {surviving_count}"
        );

        // 2. Alpha (topo-first) keeps it; beta (non-owner) has it removed.
        let alpha_has_variant = deltas[0].operations.iter().any(|op| {
            matches!(
                op,
                SchemaOperation::AddEnumVariant { enum_name, variant, .. }
                    if enum_name == "mood" && variant == "excited"
            )
        });
        let beta_has_variant = deltas[1].operations.iter().any(|op| {
            matches!(
                op,
                SchemaOperation::AddEnumVariant { enum_name, variant, .. }
                    if enum_name == "mood" && variant == "excited"
            )
        });
        assert!(
            alpha_has_variant,
            "alpha (topo-first) should retain AddEnumVariant(mood, excited)"
        );
        assert!(
            !beta_has_variant,
            "beta (non-owner) should have AddEnumVariant(mood, excited) removed"
        );

        // 3. beta has an ordering edge pointing to "alpha" (enum projection owner).
        let beta_deps = edges.get(&beta_bucket).cloned().unwrap_or_default();
        assert!(
            beta_deps.contains("alpha"),
            "beta should depend on alpha for enum variant ownership; edges: {edges:?}"
        );
    }

    /// REQ-396-7: After the first compose materialises enum ownership edges
    /// and writes model snapshots, a second compose with those snapshots as
    /// the baseline should produce no delta — `NothingToCompose`.
    #[test]
    fn recompose_after_enum_dedup_is_noop() {
        let work = temp_workspace("recompose-enum-noop");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        // Both buckets project the `mood` enum + a table that uses it.
        let mut models = BTreeMap::new();
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert(
                "mood".to_string(),
                EnumSchema {
                    name: "mood".to_string(),
                    variants: vec!["happy".to_string(), "sad".to_string()],
                },
            );
            alpha_schema.models.insert(
                "posts".to_string(),
                table_with_enum_col(&alpha_bucket, "posts", "mood", "mood"),
            );
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        {
            let mut beta_schema = empty_snapshot(&beta_bucket);
            beta_schema.enums.insert(
                "mood".to_string(),
                EnumSchema {
                    name: "mood".to_string(),
                    variants: vec!["happy".to_string(), "sad".to_string()],
                },
            );
            beta_schema.models.insert(
                "comments".to_string(),
                table_with_enum_col(&beta_bucket, "comments", "author_mood", "mood"),
            );
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Empty snapshots — fresh compose.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(alpha_bucket.clone(), empty_snapshot(&alpha_bucket));
        snapshots.insert(beta_bucket.clone(), empty_snapshot(&beta_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        // First compose — creates the enum + tables.
        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "recompose-enum-noop",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };
        let report = compose(req).expect("first compose should succeed");
        assert!(
            !report.composed_buckets.is_empty(),
            "first compose should have produced buckets"
        );

        // Collect each bucket's model_snapshot from the pending JSON
        // into a new BTreeMap to use as the baseline for recompose.
        let mut model_snapshots: BTreeMap<BucketKey, AppliedSchema> = BTreeMap::new();
        for cb in &report.composed_buckets {
            let pending = read_written_pending(&work, &report, &cb.bucket.database, &cb.bucket.app);
            model_snapshots.insert(cb.bucket.clone(), pending.model_snapshot);
        }

        // Second compose — same models, post-compose snapshots as baseline.
        let req2 = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &model_snapshots,
            apps: &apps,
            name: "recompose-enum-noop-2",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 1),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };
        let err = compose(req2).expect_err("second compose should produce NothingToCompose");
        assert!(
            matches!(err, ComposeError::NothingToCompose),
            "recompose after enum dedup should be a no-op, got: {err}"
        );
        cleanup_workspace(&work);
    }

    /// Bucket A's snapshot already records `mood`; bucket B newly
    /// references it. Assert no CREATE TYPE in any delta.
    #[test]
    fn add_enum_suppressed_when_snapshot_already_has_it() {
        let work = temp_workspace("enum-already-in-snapshot");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        let mood = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        let mut models = BTreeMap::new();
        // Both buckets project the enum.
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert("mood".to_string(), mood.clone());
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        {
            let mut beta_schema = empty_snapshot(&beta_bucket);
            beta_schema.enums.insert("mood".to_string(), mood.clone());
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Snapshots: alpha already has the enum; beta does not.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad"]),
        );
        snapshots.insert(beta_bucket.clone(), empty_snapshot(&beta_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "enum-already-in-snapshot",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        // Beta would have AddEnum for mood, but alpha's snapshot
        // already has it → suppressed everywhere.
        let result = compose(req);
        // With suppression, beta should have no-op (or at least no CREATE TYPE).
        // If both deltas become NoOp, we get NothingToCompose.
        match result {
            Ok(report) => {
                for cb in &report.composed_buckets {
                    let up = crate::migrate::read_workspace_file_to_string(&work, &cb.up_sql_path)
                        .unwrap();
                    assert!(
                        !up.contains("CREATE TYPE"),
                        "No CREATE TYPE expected when snapshot already has enum. Bucket {:?}:\n{}",
                        cb.bucket,
                        up
                    );
                }
            }
            Err(ComposeError::NothingToCompose) => {
                // Both deltas became no-ops after suppression → valid.
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
        cleanup_workspace(&work);
    }

    /// Pre-fix shape: Bucket A references `mood` (snapshot has it,
    /// projection has it → no-op). Bucket B does NOT reference `mood`
    /// (snapshot has it from pre-fix global fanout, scoped projection
    /// drops it → DropEnum in delta). Assert Bucket B's SQL contains
    /// no DROP TYPE.
    #[test]
    fn drop_enum_deferred_until_last_referencing_bucket() {
        let work = temp_workspace("defer-drop-enum");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        let mood = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        let mut models = BTreeMap::new();
        // Alpha's projection still has the enum.
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert("mood".to_string(), mood.clone());
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        // Beta's projection no longer has the enum.
        {
            let beta_schema = empty_snapshot(&beta_bucket);
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Snapshots: both have the enum (pre-fix global fanout).
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad"]),
        );
        snapshots.insert(
            beta_bucket.clone(),
            snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad"]),
        );

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "defer-drop-enum",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        // Beta has DropEnum for mood (snapshot has it, projection
        // doesn't), but alpha's projected schema still references it,
        // so the drop is deferred.
        let result = compose(req);
        match result {
            Ok(report) => {
                for cb in &report.composed_buckets {
                    let up = crate::migrate::read_workspace_file_to_string(&work, &cb.up_sql_path)
                        .unwrap();
                    assert!(
                        !up.contains("DROP TYPE"),
                        "No DROP TYPE expected when another bucket still references enum. Bucket {:?}:\n{}",
                        cb.bucket,
                        up
                    );
                }
            }
            Err(ComposeError::NothingToCompose) => {
                // Alpha is no-op (enum unchanged), Beta's DropEnum
                // was suppressed → both become no-ops. Valid.
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
        cleanup_workspace(&work);
    }

    /// No bucket references `mood` anymore; both have DropEnum.
    /// Assert exactly one DROP TYPE across buckets (first in order).
    #[test]
    fn last_bucket_drop_emits_drop_enum() {
        let work = temp_workspace("last-bucket-drop-enum");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        let mut models = BTreeMap::new();
        // Neither bucket projects the enum.
        {
            models.insert(alpha_bucket.clone(), empty_snapshot(&alpha_bucket));
        }
        {
            models.insert(beta_bucket.clone(), empty_snapshot(&beta_bucket));
        }

        // Snapshots: both have the enum (pre-fix global fanout).
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad"]),
        );
        snapshots.insert(
            beta_bucket.clone(),
            snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad"]),
        );

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "last-bucket-drop-enum",
            allow_destructive: true, // DropEnum is destructive.
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");

        // Beta keeps the drop (last bucket in topological order), so alpha's
        // DropEnum was removed and alpha has no operations.
        let alpha_present = report
            .composed_buckets
            .iter()
            .any(|c| c.bucket == alpha_bucket);
        assert!(
            !alpha_present,
            "alpha should not appear in composed_buckets after its DropEnum is deduplicated away"
        );

        // Read up SQL for beta (the keeper — last in topological order).
        let beta_up = crate::migrate::read_workspace_file_to_string(
            &work,
            &report
                .composed_buckets
                .iter()
                .find(|c| c.bucket == beta_bucket)
                .expect("beta should be in composed_buckets")
                .up_sql_path,
        )
        .unwrap();

        assert!(
            beta_up.contains("DROP TYPE"),
            "beta should emit DROP TYPE. beta_up:\n{}",
            beta_up
        );
        cleanup_workspace(&work);
    }

    /// AddEnumVariant interaction test (REQ-396-12): bucket A owns
    /// AddEnum for `mood`; bucket B has AddEnumVariant for same enum.
    /// Assert bucket B depends on bucket A.
    #[test]
    fn add_enum_variant_orders_after_owner_via_depends_on() {
        let work = temp_workspace("enum-variant-depends");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        let mood_base = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };
        let mood_extended = EnumSchema {
            name: "mood".to_string(),
            variants: vec![
                "happy".to_string(),
                "sad".to_string(),
                "excited".to_string(),
            ],
        };

        let mut models = BTreeMap::new();
        // Alpha projects the base enum.
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema
                .enums
                .insert("mood".to_string(), mood_base.clone());
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        // Beta projects the extended enum (new variant).
        {
            let mut beta_schema = empty_snapshot(&beta_bucket);
            beta_schema
                .enums
                .insert("mood".to_string(), mood_extended.clone());
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Alpha snapshot empty (no enum yet) → diff produces AddEnum.
        // Beta snapshot has base enum → diff produces AddEnumVariant for "excited".
        let mut snapshots = BTreeMap::new();
        snapshots.insert(alpha_bucket.clone(), empty_snapshot(&alpha_bucket));
        snapshots.insert(
            beta_bucket.clone(),
            snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad"]),
        );

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "enum-variant-depends",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("composes");

        // Beta depends on alpha (enum ownership edge for same-run AddEnum + AddEnumVariant).
        let beta_pending = read_written_pending(&work, &report, "main", "beta");
        assert!(
            beta_pending.depends_on.contains(&"alpha".to_string()),
            "beta should depend on alpha for enum ownership"
        );
        cleanup_workspace(&work);
    }

    /// Classification re-derivation test: delta with DropEnum
    /// (suppressed by deferral) + AddColumn → re-classifies to
    /// non-destructive. Compose succeeds WITHOUT --allow-destructive.
    #[test]
    fn classification_rederived_after_drop_enum_suppression() {
        let work = temp_workspace("reclassify-after-drop-enum");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        let mood = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        let mut models = BTreeMap::new();
        // Alpha keeps the enum + a table.
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert("mood".to_string(), mood.clone());
            alpha_schema
                .models
                .insert("posts".to_string(), table_users(&alpha_bucket));
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        // Beta drops the enum but adds a column to its own table.
        {
            let mut beta_schema = empty_snapshot(&beta_bucket);
            // No enum in beta's projection.
            beta_schema
                .models
                .insert("comments".to_string(), table_users(&beta_bucket));
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Snapshots: both have the enum; beta has the comments table
        // without the extra column.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad"]),
        );
        {
            let mut beta_snap = snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad"]);
            // Beta has the comments table already.
            beta_snap
                .models
                .insert("comments".to_string(), table_users(&beta_bucket));
            snapshots.insert(beta_bucket.clone(), beta_snap);
        }

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "reclassify-after-drop-enum",
            allow_destructive: false, // No --allow-destructive!
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        // Beta would have DropEnum (destructive) but it's suppressed
        // because alpha still references mood. After suppression, beta
        // should be NoOp or additive only → compose succeeds without
        // --allow-destructive.
        let result = compose(req);
        match result {
            Ok(_) => {
                // Success — the DropEnum was suppressed and reclassified.
            }
            Err(ComposeError::NothingToCompose) => {
                // Both deltas became no-ops — also valid.
            }
            Err(ComposeError::DestructiveRequiresAllowDestructive { .. }) => {
                panic!(
                    "DropEnum should have been suppressed and reclassified to non-destructive; \
                     compose failed with DestructiveRequiresAllowDestructive"
                )
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
        cleanup_workspace(&work);
    }

    /// Regression test: enum owner selection must follow FK-based topological
    /// order, not alphabetical. When alpha has an FK to beta, the topo order
    /// is [beta, alpha] (beta first), but alphabetical would be [alpha, beta].
    /// Beta should own the enum; alpha's AddEnum should be deduplicated away.
    #[test]
    fn enum_owner_follows_fk_order_not_alphabetical() {
        let work = temp_workspace("enum-owner-fk-order");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        // Build a column with both an enum type and an FK to beta's table.
        fn fk_to_beta_items() -> crate::migrate::schema::ForeignKeySchema {
            use crate::migrate::schema::{ForeignKeySchema, OnDeleteSchema};
            ForeignKeySchema {
                deferrable: false,
                initially_deferred: false,
                on_delete: OnDeleteSchema::Restrict,
                ref_column: "id".to_string(),
                ref_table: "items".to_string(),
            }
        }

        fn col_item_id_with_fk() -> ColumnSchema {
            ColumnSchema {
                name: "item_id".to_string(),
                sql_type: "BIGINT".to_string(),
                nullable: false,
                foreign_key: Some(fk_to_beta_items()),
                ..col("item_id", "BIGINT", false)
            }
        }

        fn col_mood() -> ColumnSchema {
            ColumnSchema {
                name: "mood".to_string(),
                sql_type: "mood".to_string(),
                nullable: true,
                foreign_key: None,
                ..col("mood", "mood", true)
            }
        }

        // Alpha's table: has enum col + FK to beta's items table.
        fn alpha_posts(bucket: &BucketKey) -> TableSchema {
            TableSchema {
                app: Some(bucket.app.clone()),
                columns: vec![id_column_heerid_desc(), col_item_id_with_fk(), col_mood()],
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
                table: "posts".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        // Beta's table: has enum col only, no FK.
        fn beta_items(bucket: &BucketKey) -> TableSchema {
            TableSchema {
                app: Some(bucket.app.clone()),
                columns: vec![id_column_heerid_desc(), col_mood()],
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
                table: "items".to_string(),
                table_comment: None,
                storage_params: None,
                tablespace: None,
                tenant_key: None,
            }
        }

        let mood = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        // Models: both buckets project the same enum + their own table.
        let mut models = BTreeMap::new();
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert("mood".to_string(), mood.clone());
            alpha_schema
                .models
                .insert("posts".to_string(), alpha_posts(&alpha_bucket));
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        {
            let mut beta_schema = empty_snapshot(&beta_bucket);
            beta_schema.enums.insert("mood".to_string(), mood.clone());
            beta_schema
                .models
                .insert("items".to_string(), beta_items(&beta_bucket));
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Snapshots: empty for both (fresh compose, no prior state).
        let mut snapshots = BTreeMap::new();
        snapshots.insert(alpha_bucket.clone(), empty_snapshot(&alpha_bucket));
        snapshots.insert(beta_bucket.clone(), empty_snapshot(&beta_bucket));

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "enum-fk-order",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 10, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        // 1. Compose must succeed (no CrossBucketForeignKeyCycle).
        let report = compose(req).expect("compose should succeed without cycle error");

        // 2. Beta's SQL contains CREATE TYPE (beta is the enum owner — first in topo order).
        let beta_up = crate::migrate::read_workspace_file_to_string(
            &work,
            &report
                .composed_buckets
                .iter()
                .find(|c| c.bucket == beta_bucket)
                .expect("beta should be in composed_buckets")
                .up_sql_path,
        )
        .unwrap();
        assert!(
            beta_up.contains("CREATE TYPE"),
            "beta should emit CREATE TYPE (enum owner). beta_up:\n{}",
            beta_up
        );

        // 3. Alpha's SQL does NOT contain CREATE TYPE (AddEnum deduplicated away).
        let alpha_cb = report
            .composed_buckets
            .iter()
            .find(|c| c.bucket == alpha_bucket)
            .expect("alpha should be in composed_buckets");
        let alpha_up =
            crate::migrate::read_workspace_file_to_string(&work, &alpha_cb.up_sql_path).unwrap();
        assert!(
            !alpha_up.contains("CREATE TYPE"),
            "alpha should NOT emit CREATE TYPE (deduplicated). alpha_up:\n{}",
            alpha_up
        );

        // 4. Alpha depends on beta (from both FK edge and enum ownership edge).
        let alpha_pending = read_written_pending(&work, &report, "main", "alpha");
        assert!(
            alpha_pending.depends_on.contains(&"beta".to_string()),
            "alpha should depend on beta (FK + enum ownership). depends_on: {:?}",
            alpha_pending.depends_on
        );
        cleanup_workspace(&work);
    }

    /// Stale snapshot convergence: a bucket whose only op is a `DropEnum`
    /// suppressed because another bucket still projects the type ends up with
    /// an empty delta after reconciliation. Its `schema_snapshot.json` must be
    /// silently advanced to the current scoped snapshot so build.rs no longer
    /// warns "run compose" on the next build.
    ///
    /// Verifies:
    /// 1. `ComposeReport::converged_snapshot_buckets` contains the bucket.
    /// 2. The snapshot file exists at the expected path and does NOT contain
    ///    the now-absent enum (the bucket no longer projects it).
    /// 3. No migration SQL file was written for the converged bucket.
    #[test]
    fn suppressed_drop_enum_writes_convergence_snapshot() {
        let work = temp_workspace("converge-snapshot-drop-enum");
        let guard = lock_for(&work);

        let alpha_bucket = BucketKey {
            database: "main".into(),
            app: "alpha".into(),
        };
        let beta_bucket = BucketKey {
            database: "main".into(),
            app: "beta".into(),
        };

        let mood = EnumSchema {
            name: "mood".to_string(),
            variants: vec!["happy".to_string(), "sad".to_string()],
        };

        // Current model state:
        //   Alpha: keeps the enum + adds a NEW table (posts) — alpha has a
        //          real delta (AddTable) so compose.rs's step-5 filter passes.
        //   Beta:  no longer projects the enum, no tables — its current scoped
        //          models are empty. The diff produces DropEnum as beta's only op.
        let mut models = BTreeMap::new();
        {
            let mut alpha_schema = empty_snapshot(&alpha_bucket);
            alpha_schema.enums.insert("mood".to_string(), mood.clone());
            // Alpha's posts table is NEW (not in snapshot) → AddTable op survives.
            alpha_schema.models.insert(
                "posts".to_string(),
                table_with_enum_col(&alpha_bucket, "posts", "mood", "mood"),
            );
            models.insert(alpha_bucket.clone(), alpha_schema);
        }
        {
            // Beta no longer projects the enum and has no tables.
            let beta_schema = empty_snapshot(&beta_bucket);
            models.insert(beta_bucket.clone(), beta_schema);
        }

        // Snapshots:
        //   Alpha: has the enum but NO posts table yet (alpha gets AddTable + AddEnum
        //          from the diff — AddEnum later suppressed by the snapshot check since
        //          beta's snapshot already has it; AddTable survives).
        //   Beta:  has the mood enum (stale from the old global-snapshot behaviour) —
        //          diff produces DropEnum which gets suppressed because alpha still
        //          projects mood. Beta's delta becomes empty → convergence.
        let mut snapshots = BTreeMap::new();
        snapshots.insert(
            alpha_bucket.clone(),
            snapshot_with_enum(&alpha_bucket, "mood", &["happy", "sad"]),
        );
        // Beta snapshot carries the mood enum (stale state). No tables in snapshot.
        snapshots.insert(
            beta_bucket.clone(),
            snapshot_with_enum(&beta_bucket, "mood", &["happy", "sad"]),
        );

        let apps: Vec<AppLifecycle> = vec![
            AppLifecycle {
                label: "alpha".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
            AppLifecycle {
                label: "beta".into(),
                database: "main".into(),
                renamed_from: None,
                tombstone: false,
            },
        ];

        let req = ComposeRequest {
            workspace_root: &work,
            models: &models,
            snapshots: &snapshots,
            apps: &apps,
            name: "converge-snapshot",
            allow_destructive: false,
            force_overwrite: false,
            now: at(2026, 6, 11, 0, 0, 0),
            _guard: &guard,
            pk_flip_join_table_option: None,
            skip_phase_zero_auto_emit: true,
        };

        let report = compose(req).expect("compose succeeds");

        // 1. Beta's bucket is recorded as converged (DropEnum was suppressed
        //    and beta's delta became empty, prompting a snapshot advance).
        assert!(
            report.converged_snapshot_buckets.contains(&beta_bucket),
            "beta should be in converged_snapshot_buckets; got: {:?}",
            report.converged_snapshot_buckets
        );

        // 2. Beta's snapshot file was written at the expected path.
        let snap_path = crate::migrate::target::snapshot_path(&work, &beta_bucket);
        assert!(
            snap_path.exists(),
            "beta schema_snapshot.json should exist after convergence: {snap_path:?}"
        );

        // 3. The snapshot does NOT contain the mood enum (beta no longer
        //    projects it — the scoped snapshot reflects current beta models).
        let snap_bytes = crate::migrate::read_workspace_file(&work, &snap_path).unwrap();
        let snap: crate::migrate::schema::AppliedSchema =
            serde_json::from_slice(&snap_bytes).expect("valid snapshot JSON");
        assert!(
            !snap.enums.contains_key("mood"),
            "beta convergence snapshot must not contain the mood enum; got: {:?}",
            snap.enums.keys().collect::<Vec<_>>()
        );

        // 4. No migration SQL file was written for beta (DropEnum suppressed,
        //    no other ops survived).
        assert!(
            !report
                .composed_buckets
                .iter()
                .any(|cb| cb.bucket == beta_bucket),
            "beta should NOT appear in composed_buckets (only snapshot was advanced)"
        );
        cleanup_workspace(&work);
    }
}
