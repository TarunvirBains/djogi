//! `djogi migrations attune` — local-history reconciliation.
//! # Scope (— amendment)
//! `attune` operates on local migration history. Three modes:
//! 1. **Default (`AttuneMode::DiffOnly`)**: read-only diff between
//!    on-disk SQL files and the ledger. Reports SQL files present on
//!    disk but absent from the ledger ("unrecorded"), and ledger rows
//!    whose corresponding SQL file is missing on disk ("orphaned").
//!    Acquires the workspace file lock to take a consistent snapshot
//!    but never writes.
//! 2. **`AttuneMode::Record`**: walks the same drift report and
//!    INSERTs ledger rows for every unrecorded SQL file, with
//!    `status = 'applied'` and a `partial_apply_note` recording the
//!    operator-supplied reason. **Does NOT execute the SQL** — `Record`
//!    is the operator asserting "these migrations were already applied
//!    out-of-band". Distinct from `fake_apply_plan` because `Record`
//!    walks every unrecorded SQL file in the bucket in one go.
//! 3. **`AttuneMode::Squash { from, publish, app }`**: HISTORY REWRITE.
//!    Coalesces every committed SQL file from `from` to HEAD into one
//!    squashed file, deletes the originals, and removes the deleted
//!    versions from the ledger. Per plus the fixup for (single-bucket scope):
//! - **Localhost-only.** Refuses to run when `DATABASE_URL` does
//!   not resolve to the local machine — see
//!   [`crate::migrate::policy::is_localhost_connection`]. A typo in
//!   the URL pointing at staging cannot rewrite history that other
//!   developers also pull from.
//! - **Dev-profile-only.** Refuses to run when
//!   `Djogi.toml::profile = "production"`. Production environments
//!   have a hard line against destructive history rewrites.
//! - **Local-only by default.** The `--publish` flag must be
//!   explicitly passed for the squashed history to be pushed to
//!   the remote `migrations` submodule. Without it, the rewrite
//!   stays local — the operator can inspect the result, run the
//!   test suite, and only then publish.
//! - **`--publish` is atomic: commit-then-push.**
//!   When `--publish` is set, attune treats the squash mutation as
//!   one atomic step: it stages every change in the migrations
//!   working tree (`git add -A`), creates a commit
//!   (`djogi attune --squash from <from>`), then pushes the current
//!   branch to `origin`. The operator does NOT need to commit the
//!   mutation themselves — the publisher's contract is "commit the
//!   squash mutation, then push to origin". This matches 's
//!   "destructive history rewrite" framing: the squash mutation +
//!   the publish are one indivisible operation. Without `--publish`
//!   the operator owns the commit cycle as before.
//! # File-lock contract
//! Every mode acquires the workspace [`super::guard::WorkspaceGuard`]
//! before touching any path. Concurrent compose / apply / repair
//! invocations cannot interleave with attune.
//! # Database scope
//! Each attune invocation is bound to ONE database — the active
//! `DjogiContext`'s `current_database()`. The disk scan is filtered
//! to `migrations/<active_db>/...` and ledger queries run against the
//! connected pool. Multi-database workspaces require running attune
//! once per database; this is intentional and matches the existing
//! single-context architecture (the runner has a single pool today;
//! `pool_for(database)` is a future seam).
//! # Read-only dry-run contract
//! Attune's read-only contract extends to the ledger table itself.
//! Three paths are read-only and MUST NOT bootstrap
//! `djogi_schema_migrations`:
//! - `AttuneMode::DiffOnly` (any value of `apply`)
//! - `AttuneMode::Record { .. }` with `apply == false`
//! - `AttuneMode::Squash { .. }` with `apply == false`
//!   On a fresh database where the ledger does not exist yet, every
//!   read-only path probes `pg_class` instead of calling
//!   [`super::ledger::bootstrap`]; the returned report carries an
//!   [`AttuneDiagnostic::LedgerTableMissing`] entry and the ledger
//!   stays absent. The operator must run `apply` or `attune --record
//! --apply` (or `baseline`) to bootstrap. Only Record / Squash with
//!   `--apply` call `ledger::bootstrap`.
//!   , Record / Squash bootstrapped the ledger up-front
//!   regardless of `--apply`, which silently created the table during
//!   a dry-run — an out-of-contract mutation. The fix gates the bootstrap behind `req.apply`.
//! # No regex
//! Per the Djogi-wide no-regex rule, the SQL filename detection uses
//! byte-level prefix / suffix checks against the [`super::naming`]
//! module's emitted shapes. A "version" is recovered via
//! [`super::naming::version_id`] / [`super::naming::version_prefix`]
//! by walking the leading `V` then digits / underscores prefix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::context::DjogiContext;
use crate::error::DjogiError;

use super::bootstrap::PHASE_ZERO_VERSION;
use super::guard::WorkspaceGuard;
use super::ledger::{
    self, ExecutionMode, LedgerRow, LedgerStatus, SHA256_HEX_LEN, compute_checksum,
};
use super::naming::{down_filename, up_filename};
use super::phase_zero::{PhaseZeroArtifactState, classify_phase_zero_artifact};
use super::projection::BucketKey;
use super::replay_plan::committed_replay_plan_filename;
use super::reset::{
    ResetSqlSide, compute_committed_down_sql_checksum, compute_committed_sql_checksum,
};
use super::schema::SNAPSHOT_FORMAT_VERSION;
use super::target::bucket_dir;

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
    /// **`publish`** is the second gate: when `false`, the rewrite
    /// stays local. When `true`, attune shells out to
    /// `git -C <migrations_root> push` to publish. Default is
    /// `false`; a missing `--publish` flag NEVER auto-publishes.
    /// **`app`** narrows the squash to a single bucket within the
    /// connected database. When `None`, the bucket is auto-
    /// detected: if exactly one bucket contains `from`, that bucket
    /// is the squash target. If `from` exists in MULTIPLE buckets,
    /// auto-detection refuses with
    /// [`AttuneRefusal::SquashFromVersionAmbiguous`] and the operator
    /// must re-run with an explicit `app`. The pre-implementation
    /// applied the `versions >= from` predicate to every bucket in the
    /// workspace, which collapsed unrelated bucket histories — a
    /// destructive, non-recoverable side effect.
    Squash {
        /// Inclusive starting version (e.g. `V20260101000000__init`).
        from: String,
        /// `true` to push to the migrations submodule's remote;
        /// `false` to keep the rewrite local.
        publish: bool,
        /// Optional explicit app label to scope the squash. `None`
        /// means "auto-detect"; an explicit value forces the squash
        /// to that single bucket and refuses if the version is not
        /// present there.
        app: Option<String>,
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
    /// Resolved Git SHA of the operator-supplied target. `Some(sha)`
    /// when `target` was supplied AND resolution succeeded; `None`
    /// otherwise. Operators consume this to confirm the rev-parse
    /// landed on the SHA they expected.
    pub resolved_target: Option<String>,
    /// `true` when the parent repo's recorded submodule pointer was
    /// updated to `resolved_target`. Only set when both `record` AND
    /// `apply` were `true` in the request, AND the target resolved
    /// successfully, AND the parent index update succeeded.
    pub parent_pointer_updated: bool,
    /// Structured diagnostics surfaced alongside the diff entries.
    /// Today this carries [`AttuneDiagnostic::LedgerTableMissing`] on
    /// every read-only path — DiffOnly always, plus Record / Squash
    /// without `--apply` — when the ledger has not been bootstrapped
    /// on the connected database. The report is still produced, but
    /// the operator sees an actionable note that they must run
    /// `apply` or `attune --record --apply` to bootstrap before
    /// subsequent diffs are meaningful. The report also carries
    /// [`AttuneDiagnostic::DryRunMutationsSkipped`] when a Record /
    /// Squash mode was invoked WITHOUT `--apply`: the diff is
    /// reported but no DB / disk mutation happened.
    pub diagnostics: Vec<AttuneDiagnostic>,
}

/// Structured operator-facing diagnostic surfaced by [`attune`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttuneDiagnostic {
    /// The `djogi_schema_migrations` table does not exist in the
    /// connected database. Attune's read-only dry-run contract means
    /// every dry-run path — DiffOnly always, plus Record / Squash
    /// WITHOUT `--apply` — does NOT bootstrap the ledger. The
    /// operator must run `apply` or
    /// `attune --record --apply` first.
    /// The `database` field is the active connection's
    /// `current_database()` so a multi-database workspace's diagnostic
    /// is unambiguous.
    LedgerTableMissing { database: String },
    /// A Record or Squash mode was invoked without `--apply`. The
    /// diff was computed and reported but no DB / disk mutation
    /// happened. The operator re-runs with `--apply` to commit. The
    /// `mode` field surfaces the mode the operator asked for so the
    /// message is unambiguous in mixed-mode runs.
    DryRunMutationsSkipped { mode: &'static str },
    /// Recording was effective (via `--record` or auto-implied by
    /// `--squash`) but `--apply` was absent. The would-be
    /// parent submodule pointer update is described but not actually
    /// written. Distinct from `DryRunMutationsSkipped` so the operator
    /// sees both messages when they pass `--record` (or `--squash`)
    /// without `--apply`.
    DryRunRecordSkipped { resolved_target: Option<String> },
    /// Squash-implied recording was effective but `--apply` was absent.
    /// Separate variant so squash wording can stay neutral while the
    /// explicit `--record` path can name the causal flag.
    DryRunSquashRecordSkipped { resolved_target: Option<String> },
}

impl AttuneDiagnostic {
    /// Stable code used by CLI rendering. Kept short so terminal
    /// output stays compact.
    pub fn code(&self) -> &'static str {
        match self {
            AttuneDiagnostic::LedgerTableMissing { .. } => "ATTUNE-001",
            AttuneDiagnostic::DryRunMutationsSkipped { .. } => "ATTUNE-002",
            AttuneDiagnostic::DryRunRecordSkipped { .. }
            | AttuneDiagnostic::DryRunSquashRecordSkipped { .. } => "ATTUNE-003",
        }
    }
}

impl std::fmt::Display for AttuneDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttuneDiagnostic::LedgerTableMissing { database } => write!(
                f,
                "[{}] ledger table `djogi_schema_migrations` does not exist in database \
                 `{database}`; ledger not bootstrapped — run `migrations apply` or \
                 `attune --record --apply` first; plain `attune --record` without `--apply` \
                 is dry-run after U-5 and will not bootstrap; the apply flag is load-bearing",
                self.code(),
            ),
            AttuneDiagnostic::DryRunMutationsSkipped { mode } => write!(
                f,
                "[{}] {mode} mode requested without `--apply`; diff reported, \
                 no database or disk mutation happened — re-run with `--apply` to commit",
                self.code(),
            ),
            AttuneDiagnostic::DryRunRecordSkipped { resolved_target } => match resolved_target {
                Some(sha) => write!(
                    f,
                    "[{}] `--record` was requested without `--apply`; parent submodule \
                     pointer would be updated to `{sha}` but no parent index mutation \
                     happened — re-run with `--apply` to commit",
                    self.code(),
                ),
                None => write!(
                    f,
                    "[{}] `--record` was requested without `--apply`; no parent submodule \
                     pointer would be updated because no target was resolved — pass \
                     `--target <ref>` to populate, then re-run with `--apply` to commit",
                    self.code(),
                ),
            },
            AttuneDiagnostic::DryRunSquashRecordSkipped { resolved_target } => {
                match resolved_target {
                    Some(sha) => write!(
                        f,
                        "[{}] would update parent submodule pointer to `{sha}` but `--apply` \
                     was not provided; no parent index mutation happened — re-run with \
                     `--apply` to commit",
                        self.code(),
                    ),
                    None => write!(
                        f,
                        "[{}] no parent submodule pointer would be updated because no target \
                     was resolved; pass `--target <ref>` to populate, then re-run with \
                     `--apply` to commit",
                        self.code(),
                    ),
                }
            }
        }
    }
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
    /// Target resolution failed. Either the local `git rev-parse
    /// <target>` failed AND the remote fetch + retry also failed,
    /// or the migrations submodule has no remote configured and the
    /// target is not locally available.
    GitTargetResolveFailed {
        target: String,
        stderr: String,
        status_code: Option<i32>,
    },
    /// `git fetch` failed during target resolution. The local resolution failed; the fetch attempt also
    /// failed (network, auth, missing remote, …). The captured stderr
    /// names the precise failure.
    GitFetchFailed {
        stderr: String,
        status_code: Option<i32>,
    },
    /// Updating the parent repo's recorded submodule pointer
    /// failed. The new SHA was resolved successfully but `git
    /// update-index --cacheinfo` against the parent repo returned
    /// non-zero. Most often: the parent has uncommitted staged
    /// changes to the submodule path that conflict with the
    /// intended pointer write.
    GitUpdateSubmodulePointerFailed {
        new_sha: String,
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
    /// `Squash` mode was invoked but `Djogi.toml::[database].dev_mode`
    /// is `false`. Per `docs/spec/configuration.md` §14 the squash
    /// gate is the conjunction of three conditions: localhost,
    /// `dev_mode = true`, and `DJOGI_ENV != "production"`.
    /// the `dev_mode` half was documented but never read; this variant
    /// surfaces a refusal that names the field the operator must flip
    /// to opt-in to local history rewrites.
    SquashDevModeOff,
    /// `Squash` mode was invoked but the `DJOGI_ENV` environment
    /// variable is set to `"production"` (case-insensitive ASCII
    /// compare). The env var is the deployment-time signal — it
    /// overrides the `Djogi.toml` profile when CI / orchestration sets
    /// it before invoking `djogi`. Refusing on the env var means a
    /// staging cluster running with `DJOGI_ENV=production` cannot
    /// accidentally rewrite local migration history even if a developer
    /// shells in and runs `attune --squash`.
    SquashEnvIsProduction { env_value: String },
    /// `Squash --from` named a version that does not exist on disk in
    /// any bucket scoped to the connected database. The squash
    /// refuses rather than silently no-op because the version argument
    /// is load-bearing for an audit trail.
    SquashFromVersionNotFound { version: String },
    /// `Squash --from` named a version that exists in MULTIPLE
    /// buckets within the connected database. Squash refuses to
    /// silently pick one because each bucket's history is independent;
    /// the operator must disambiguate via `--app <app_label>`.
    /// The `buckets` field carries `(database, app)` pairs in
    /// `database/app` rendering form so the error message lists every
    /// candidate.
    SquashFromVersionAmbiguous {
        version: String,
        buckets: Vec<String>,
    },
    /// A Phase 0 migration artifact (`V00000000000000__phase_zero_bootstrap`)
    /// was classified as seed-capable runtime-only, seed-DML non-runtime,
    /// generated-stale, or ambiguous. Attune refuses to record, insert ledger
    /// rows, rewrite files, or update parent pointers for stale Phase 0
    /// artifacts. The operator must regenerate the Phase 0 file from the
    /// canonical composer before retrying.
    StalePhaseZero { version: String },
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
            AttuneError::GitTargetResolveFailed {
                target,
                stderr,
                status_code,
            } => match status_code {
                Some(c) => write!(
                    f,
                    "attune target `{target}`: git rev-parse failed locally and after \
                     a remote fetch retry (status {c}): {stderr}; either the target \
                     does not exist on any configured remote, or `migrations/` has no \
                     remote at all"
                ),
                None => write!(
                    f,
                    "attune target `{target}`: git rev-parse terminated by signal: {stderr}"
                ),
            },
            AttuneError::GitFetchFailed {
                stderr,
                status_code,
            } => match status_code {
                Some(c) => write!(
                    f,
                    "attune target resolution: git fetch failed (status {c}): {stderr}"
                ),
                None => write!(
                    f,
                    "attune target resolution: git fetch terminated by signal: {stderr}"
                ),
            },
            AttuneError::GitUpdateSubmodulePointerFailed {
                new_sha,
                stderr,
                status_code,
            } => match status_code {
                Some(c) => write!(
                    f,
                    "attune --record: failed to update parent repo's submodule \
                     pointer to `{new_sha}` (status {c}): {stderr}"
                ),
                None => write!(
                    f,
                    "attune --record: parent submodule pointer update terminated \
                     by signal: {stderr}"
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
            AttuneRefusal::SquashDevModeOff => f.write_str(
                "attune --squash refuses to run when `[database].dev_mode = false` \
                 in Djogi.toml; squash is a destructive history rewrite and the \
                 `dev_mode` flag is the explicit opt-in. Set `dev_mode = true` in \
                 the `[database]` block before retrying",
            ),
            AttuneRefusal::SquashEnvIsProduction { env_value } => write!(
                f,
                "attune --squash refuses to run when DJOGI_ENV=`{env_value}` \
                 (case-insensitive `production`); the deployment-environment \
                 signal overrides any local Djogi.toml profile override. Unset \
                 DJOGI_ENV (or set it to a non-production value) before retrying"
            ),
            AttuneRefusal::SquashFromVersionNotFound { version } => write!(
                f,
                "attune --squash --from `{version}` did not match any version on disk \
                 in the connected database; list `migrations/<database>/<app>/` to find \
                 a valid starting version"
            ),
            AttuneRefusal::SquashFromVersionAmbiguous { version, buckets } => write!(
                f,
                "attune --squash --from `{version}` exists in multiple buckets ({}); \
                 squash refuses to silently pick one — each bucket's history is \
                 independent. Disambiguate by passing `--app <app_label>` to scope the \
                 squash to a single bucket",
                buckets.join(", ")
            ),
            AttuneRefusal::StalePhaseZero { version } => write!(
                f,
                "attune refused — Phase 0 artifact `{version}` is seed-capable, \
                 seed-DML non-runtime, generated-stale, or ambiguous; \
                 regenerate the Phase 0 file from the canonical composer before retrying"
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
            // The new git-related variants do not wrap a `std::io::Error`
            // we can return as a source — they wrap a captured stderr string
            // from the child git process. The Display impl is the
            // operator-actionable surface.
            AttuneError::GitTargetResolveFailed { .. }
            | AttuneError::GitFetchFailed { .. }
            | AttuneError::GitUpdateSubmodulePointerFailed { .. } => None,
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
    /// `[database].dev_mode` flag from `Djogi.toml`. Squash mode
    /// refuses when this is `false` per `docs/spec/configuration.md`
    /// §14 — the third squash gate alongside localhost and dev
    /// profile. Read only for [`AttuneMode::Squash`]; ignored by
    /// `DiffOnly` and `Record`.
    pub dev_mode: bool,
    /// Optional Git target to attune the local migration history to
    /// a local or remote commit / tag / branch (per
    /// `docs/spec/configuration.md` §14). When `None`, attune
    /// reconciles against the current on-disk state.
    /// **Resolution order.** The target string is resolved first
    /// against the local migrations submodule (`git rev-parse <target>`).
    /// If that fails, attune fetches every configured remote
    /// (`git fetch --all`) and retries the resolution. A target that
    /// resolves locally never triggers a fetch — operators who deliberately
    /// kept their submodule offline can still attune to a known-local
    /// SHA.
    /// **No regex.** The target string is passed verbatim to
    /// `git rev-parse`; Djogi never parses it.
    pub target: Option<&'a str>,
    /// `true` when the operator passed `--apply`. Without `--apply`,
    /// attune is read-only — it scans, prints the diff, and exits
    /// without inserting / deleting ledger rows or updating the
    /// parent repo's submodule pointer (per
    /// `docs/spec/configuration.md` §14: "does not mutate the
    /// database unless `--apply` is explicitly passed"). The dry-run
    /// path is the operator-friendly default — review the proposed
    /// mutation before committing to it.
    pub apply: bool,
    /// `true` when the operator passed `--record`. After a successful
    /// attunement, attune updates the parent repo's recorded
    /// submodule pointer to the resolved target SHA (per
    /// `docs/spec/configuration.md` §14). `--record` is orthogonal to
    /// `--apply`: `--record` without `--apply` is a dry-run that
    /// prints the would-be parent pointer update without touching the
    /// parent index. Without `--apply`, the parent pointer is NEVER
    /// mutated regardless of `--record`.
    /// `--squash` implies recording per
    /// `docs/spec/configuration.md` §15 and `docs/spec/migrations.md`
    /// §"migrations attune". The runtime computes an
    /// `effective_record` boolean as `req.record ||
    /// matches!(req.mode, AttuneMode::Squash { .. })`, so an operator
    /// running `attune --squash --apply --target <ref>` gets the
    /// parent pointer write WITHOUT also typing `--record`. The
    /// explicit `req.record` flag is still honoured (and required)
    /// for `Record` / `DiffOnly` modes.
    pub record: bool,
    /// Mode selector — see [`AttuneMode`].
    pub mode: AttuneMode,
    /// Witness-typed proof that the workspace lock is held. Attune
    /// requires it for all three modes — even `DiffOnly` takes the
    /// lock so a concurrent compose / apply cannot mutate the tree
    /// mid-scan.
    pub _guard: &'a WorkspaceGuard,
}

// ── DJOGI_ENV gate ───────────────────────────────────────────────────────

/// Returns `Some(value)` when the `DJOGI_ENV` env var is set to a
/// case-insensitive `"production"` byte sequence; `None` otherwise.
/// **Why a separate helper.** The squash gate refuses in this
/// case. Pulling the comparison into a free function keeps the
/// gate's intent obvious at the call site and gives unit tests a
/// stable surface to pin the case-insensitive ASCII compare without
/// allocating a `to_lowercase` String.
/// **No regex.** Delegates to [`super::policy::ascii_eq_ignore_case`] so the
/// byte-level comparison lives in one place. The env-var payload is read once,
/// compared, and dropped.
fn djogi_env_is_production() -> Option<String> {
    let raw = std::env::var("DJOGI_ENV").ok()?;
    if super::policy::ascii_eq_ignore_case(raw.as_bytes(), b"production") {
        Some(raw)
    } else {
        None
    }
}

/// Push the variant-correct `DryRun*RecordSkipped` diagnostic for a
/// dry-run that would have updated the parent submodule pointer.
/// Centralises the routing rule the three dry-run paths (DiffOnly,
/// Record, Squash) all need:
/// - explicit `--record` → `DryRunRecordSkipped` (causal prose names
///   the flag);
/// - `--squash`-implied recording → `DryRunSquashRecordSkipped`
///   (neutral prose);
/// - any other mode without a resolved target → no-op.
fn push_record_skipped(
    diagnostics: &mut Vec<AttuneDiagnostic>,
    req: &AttuneRequest<'_>,
    resolved_target: &Option<String>,
) {
    if resolved_target.is_none() {
        return;
    }
    if req.record {
        diagnostics.push(AttuneDiagnostic::DryRunRecordSkipped {
            resolved_target: resolved_target.clone(),
        });
    } else if matches!(req.mode, AttuneMode::Squash { .. }) {
        diagnostics.push(AttuneDiagnostic::DryRunSquashRecordSkipped {
            resolved_target: resolved_target.clone(),
        });
    }
}

// ── Public entry point ────────────────────────────────────────────────────

/// Run `attune` against the workspace.
/// **Witness-typed lock.** The `_guard: &WorkspaceGuard` parameter is
/// the same witness pattern as [`super::runner::apply_plan`]. The
/// caller proves it acquired the workspace lock before invoking
/// attune; the runner trusts the witness without re-acquiring.
/// **No SQL execution.** Attune never runs user DDL. `Record` mode
/// inserts ledger rows; `Squash` mode rewrites files + the ledger.
/// `DiffOnly` mode is read-only.
pub async fn attune(
    ctx: &mut DjogiContext,
    req: AttuneRequest<'_>,
) -> Result<AttuneReport, AttuneError> {
    // Squash mode runs all its gates BEFORE any I/O so a refusal
    // produces zero side effects. Per `docs/spec/configuration.md`
    // §14 the gate is the conjunction of FOUR conditions, all enforced
    // here in deterministic order so an operator-facing refusal is
    // single-cause and reproducible:
    // 1. `database_url` resolves to localhost.
    // 2. `Djogi.toml::profile != "production"`.
    // 3. `Djogi.toml::[database].dev_mode == true` — was documented but
    // now enforced.
    // 4. `DJOGI_ENV` env var is NOT case-insensitive `"production"`
    // env-var override for the deployment environment.
    // The order is gate-1 → gate-4 deliberately: failing the cheapest
    // checks first means an operator typo on URL or profile surfaces
    // before the more nuanced env-var probe.
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
        if !req.dev_mode {
            return Err(AttuneError::Refused(AttuneRefusal::SquashDevModeOff));
        }
        if let Some(env_value) = djogi_env_is_production() {
            return Err(AttuneError::Refused(AttuneRefusal::SquashEnvIsProduction {
                env_value,
            }));
        }
    }

    // Resolve the active database name first — every subsequent
    // disk-scan + ledger-query path is scoped to the bucket that
    // matches THIS context's database. The CLI orchestrator runs
    // attune once per database; this guarantees an attune invocation
    // never inserts ledger rows for a database it isn't actually
    // connected to.
    // Reading `current_database()` does not require the ledger table,
    // so we issue it BEFORE the bootstrap dispatch below — DiffOnly's
    // read-only contract hinges on us not creating the ledger
    // table when the operator only asked for a diff.
    let active_database = active_database_name(ctx).await?;

    // Probe ledger existence without mutating. Apply-mode Record/Squash paths
    // bootstrap later, after Phase 0 stale-artifact preflight has passed.
    let ledger_exists = ledger_table_exists(ctx).await?;

    // Walk disk + ledger. Disk scan is filtered to ONLY the active
    // database's bucket directories: if `migrations/main/...`
    // and `migrations/crud_log/...` both exist on disk but the
    // operator's context is connected to `main`, only `main`'s tree
    // is in scope. Multi-database attune requires per-database
    // invocations.
    let disk = scan_disk_for_database(req.workspace_root, &active_database)?;
    let ledger_versions = if ledger_exists {
        scan_ledger(ctx, &active_database).await?
    } else {
        BTreeMap::new()
    };

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

    let mut diagnostics: Vec<AttuneDiagnostic> = Vec::new();
    // `LedgerTableMissing` surfaces on EVERY dry-run path that observed
    // a missing ledger table — DiffOnly always, plus Record / Squash
    // without `--apply`. Pre-fix only DiffOnly emitted the diagnostic;
    // an operator running `attune --record-ledger` (no --apply) on a fresh
    // DB had no operator-facing hint that the diff was vacuous because the
    // ledger had never been bootstrapped. The flag is the observed-emptiness
    // signal, not the mode itself.
    if !ledger_exists {
        diagnostics.push(AttuneDiagnostic::LedgerTableMissing {
            database: active_database.clone(),
        });
    }

    // Resolve the operator-supplied target (if any) before any DB / disk
    // mutation. Resolution is read-only — local first, fetch + retry only
    // when the local rev-parse failed. A resolution failure surfaces as a
    // typed error BEFORE any mutation path runs so the operator-facing
    // message names the missing target instead of a partial side effect.
    let resolved_target = match req.target {
        Some(t) if !t.is_empty() => Some(resolve_git_target(req.workspace_root, t)?),
        _ => None,
    };

    let mut report = AttuneReport {
        entries: Vec::new(),
        mutated: false,
        squashed_to: None,
        published: false,
        resolved_target: resolved_target.clone(),
        parent_pointer_updated: false,
        diagnostics,
    };

    // Dry-run gate. Mutations only happen when `--apply` is set. Without
    // it, Record / Squash skip every ledger / disk mutation and the diff
    // is the only output.
    let apply = req.apply;

    // `--squash` clearly implies recording per `docs/spec/configuration.md`
    // §15 and `docs/spec/migrations.md` §"migrations attune". The
    // effective-record boolean closes the gap: a Squash mode with a
    // resolved target writes the pointer regardless of whether `--record`
    // was passed explicitly. The operator's explicit `--record` still works
    // as before for Record / DiffOnly modes.
    // Squash genuinely needs the parent pointer to track the rewritten
    // history. After `--publish` lands the new HEAD on `origin`, the
    // parent's recorded pointer would otherwise still reference a SHA the
    // migrations submodule no longer carries.
    let effective_record = req.record || matches!(req.mode, AttuneMode::Squash { .. });

    match &req.mode {
        AttuneMode::DiffOnly => {
            entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            report.entries = entries;
            // Surface the would-be pointer update as a structured
            // diagnostic so the operator knows nothing was written when
            // --record is passed without --apply.
            if !apply {
                push_record_skipped(&mut report.diagnostics, &req, &resolved_target);
            }
            // When `--record` AND `--apply` are both set on a
            // DiffOnly run with a resolved target, write the parent
            // submodule pointer. DiffOnly itself never mutates the DB,
            // but `--record` is orthogonal — it touches the parent
            // repo's index, not the connected database.
            if req.record
                && apply
                && let Some(sha) = resolved_target.as_ref()
            {
                update_parent_submodule_pointer(req.workspace_root, sha)?;
                report.parent_pointer_updated = true;
            }
            Ok(report)
        }
        AttuneMode::Record { reason } => {
            if requires_phase_zero_attune_preflight(&req.mode, apply) {
                let mut will_insert = false;
                for entry in entries
                    .iter()
                    .filter(|entry| entry.kind == AttuneEntryKind::Unrecorded)
                {
                    if let Some(path) = disk.get(&entry.bucket).and_then(|m| m.get(&entry.version))
                    {
                        check_phase_zero_attune_entry(path, &entry.version)?;
                        will_insert = true;
                    }
                }
                if will_insert {
                    ledger::bootstrap(ctx)
                        .await
                        .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
                }
            }

            // Reuse the diff entries but split: every Unrecorded gets
            // an INSERT + flips to Recorded; Orphaned passes through
            // unchanged. The INSERT only fires when `--apply` is set;
            // without it we surface the would-be entries without writing.
            let mut out: Vec<AttuneEntry> = Vec::new();
            for entry in entries {
                match entry.kind {
                    AttuneEntryKind::Unrecorded => {
                        let path = disk
                            .get(&entry.bucket)
                            .and_then(|m| m.get(&entry.version))
                            .cloned();
                        if let Some(path) = path {
                            if apply {
                                insert_recorded_row(
                                    ctx,
                                    &entry.bucket,
                                    &entry.version,
                                    &path,
                                    reason,
                                )
                                .await?;
                                report.mutated = true;
                                out.push(AttuneEntry {
                                    kind: AttuneEntryKind::Recorded,
                                    ..entry
                                });
                                continue;
                            }
                            // Dry-run: surface the entry as still
                            // Unrecorded so the operator sees what
                            // would change. Annotate later via the
                            // DryRunMutationsSkipped diagnostic.
                            out.push(AttuneEntry {
                                kind: AttuneEntryKind::Unrecorded,
                                ..entry
                            });
                            continue;
                        }
                        out.push(entry);
                    }
                    _ => out.push(entry),
                }
            }
            out.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            report.entries = out;

            // Diagnostics for the dry-run path. If there were no
            // Unrecorded entries to record, we still surface
            // DryRunMutationsSkipped because the operator's intent
            // was to mutate.
            if !apply {
                report
                    .diagnostics
                    .push(AttuneDiagnostic::DryRunMutationsSkipped { mode: "Record" });
                push_record_skipped(&mut report.diagnostics, &req, &resolved_target);
            } else if req.record {
                // `--apply --record` with a resolved target writes the
                // parent submodule pointer AFTER the ledger inserts
                // succeeded. Without a resolved target we have nothing
                // to record (the operator can still re-run with
                // `--target <ref>` to populate it).
                if let Some(sha) = &resolved_target {
                    update_parent_submodule_pointer(req.workspace_root, sha)?;
                    report.parent_pointer_updated = true;
                }
            }
            Ok(report)
        }
        AttuneMode::Squash { from, publish, app } => {
            // Squash mutates BOTH the ledger and on-disk files. Mutation
            // only happens when --apply is set; without it we report the
            // would-be squash and exit clean.
            if !apply {
                entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
                report.entries = entries;
                report
                    .diagnostics
                    .push(AttuneDiagnostic::DryRunMutationsSkipped { mode: "Squash" });
                // Surface the would-be parent-pointer skip on Squash
                // dry-run too. Squash implies recording, so an operator
                // running `attune --squash --target <ref>` (no --apply)
                // should see "would record SHA X" alongside the dry-run
                // mutations notice.
                if effective_record {
                    push_record_skipped(&mut report.diagnostics, &req, &resolved_target);
                }
                return Ok(report);
            }
            let bucket = locate_squash_target(&disk, from, app.as_deref())?.clone();
            if requires_phase_zero_attune_preflight(&req.mode, apply)
                && let Some(from_path) = disk
                    .get(&bucket)
                    .and_then(|versions| versions.get(&from.to_string()))
            {
                check_phase_zero_attune_entry(from_path, from)?;
            }
            ledger::bootstrap(ctx)
                .await
                .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
            run_squash(
                ctx,
                req.workspace_root,
                from,
                *publish,
                app.as_deref(),
                &disk,
                &mut report,
                entries,
            )
            .await?;
            // After a successful squash the parent submodule pointer is
            // updated when a target was resolved — `--squash` implies
            // `--record`, so we gate on `effective_record` rather than
            // `req.record`. The operator no longer needs to type both
            // flags; one does the work of two per the spec contract.
            if effective_record && let Some(sha) = &resolved_target {
                update_parent_submodule_pointer(req.workspace_root, sha)?;
                report.parent_pointer_updated = true;
            }
            Ok(report)
        }
    }
}

// ── Disk scan ─────────────────────────────────────────────────────────────

/// Walk `migrations/<database>/<app>/` and return every up-SQL file's
/// `(version → path)` map, keyed by bucket.
/// **Up files only.** A migration directory contains both up and down
/// SQL files; the up file is the canonical artifact (and its presence
/// is what `attune --record` recovers from). The down file is paired
/// 1:1 — a missing down for a present up surfaces in compose's
/// idempotency check, not here.
/// **Single-database scope.** When `database_filter` is `Some`, only
/// buckets whose `database` field matches the filter are included. `None`
/// includes every bucket — used by unit tests that exercise the raw scan;
/// the production `attune` entry point always passes a filter so it never
/// inserts ledger rows for a database it isn't connected to.
fn scan_disk_filtered(
    workspace_root: &Path,
    database_filter: Option<&str>,
) -> Result<BTreeMap<BucketKey, BTreeMap<String, PathBuf>>, AttuneError> {
    super::target::scan_filesystem_with_files(workspace_root, database_filter)
        .map_err(|source| AttuneError::FilesystemScanFailed { source })
}

/// Wrapper around [`scan_disk_filtered`] that includes every bucket
/// regardless of database. Retained for unit tests that pre-date the
/// Single-database scope; production callers use
/// [`scan_disk_for_database`].
#[cfg(test)]
fn scan_disk(
    workspace_root: &Path,
) -> Result<BTreeMap<BucketKey, BTreeMap<String, PathBuf>>, AttuneError> {
    scan_disk_filtered(workspace_root, None)
}

/// Wrapper around [`scan_disk_filtered`] that scopes the scan to a
/// single database. This routes every production attune call through this
/// helper so the disk scan never returns rows whose `database` field
/// disagrees with the connected context.
fn scan_disk_for_database(
    workspace_root: &Path,
    database: &str,
) -> Result<BTreeMap<BucketKey, BTreeMap<String, PathBuf>>, AttuneError> {
    scan_disk_filtered(workspace_root, Some(database))
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
    database: &str,
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
    // bucket from which the runner derived the row. The current
    // single-pool arrangement treats the active connection as one
    // database; the bucket
    // identity reduces to `(database, app_label)`. The `database`
    // argument is the active connection's `current_database`
    // resolved once at the top of `attune` so DiffOnly never bootstraps
    // the ledger and so multi-database workspaces require a
    // fresh attune invocation per database.

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
            database: database.to_string(),
            app: app_label,
        };
        out.entry(bucket).or_default().insert(version, status);
    }
    Ok(out)
}

/// Read the active database's name from `current_database()`. Used to
/// stamp the bucket identity on ledger rows when reading them back.
/// The active database IS the bucket database in the current single-pool
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

/// Probe `pg_class` for the `djogi_schema_migrations` table without
/// creating it. Used by [`AttuneMode::DiffOnly`] so a fresh
/// database stays read-only — the prior implementation called
/// `ledger::bootstrap` unconditionally, which silently created the
/// ledger table and broke the read-only contract.
async fn ledger_table_exists(ctx: &mut DjogiContext) -> Result<bool, AttuneError> {
    let row = ctx
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_class \
             WHERE relname = 'djogi_schema_migrations' AND relkind = 'r')",
            &[],
        )
        .await
        .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
    let exists: bool = row.try_get(0).map_err(|e| AttuneError::LedgerQueryFailed {
        source: DjogiError::from(e),
    })?;
    Ok(exists)
}

// ── Record mode ───────────────────────────────────────────────────────────

fn requires_phase_zero_attune_preflight(mode: &AttuneMode, apply: bool) -> bool {
    apply && matches!(mode, AttuneMode::Record { .. } | AttuneMode::Squash { .. })
}

/// Check whether a Phase 0 candidate file is stale and refuse before
/// any attune mutation (bootstrap, ledger INSERT, parent pointer).
/// Returns `Ok(())` when the version is not Phase 0 or when Phase 0
/// is identity-free replay-current, missing, or incomplete.
fn check_phase_zero_attune_entry(up_path: &Path, version: &str) -> Result<(), AttuneError> {
    // Only check files that are Phase 0 candidates.
    if version != PHASE_ZERO_VERSION {
        return Ok(());
    }
    let bytes = std::fs::read(up_path).map_err(|e| AttuneError::SqlReadFailed {
        path: up_path.to_path_buf(),
        source: e,
    })?;
    match classify_phase_zero_artifact(&bytes) {
        PhaseZeroArtifactState::IdentityFreeCurrent
        | PhaseZeroArtifactState::Missing
        | PhaseZeroArtifactState::Incomplete => Ok(()),
        PhaseZeroArtifactState::SeedCapableRuntimeCurrent
        | PhaseZeroArtifactState::SeedDmlNotRuntimeCurrent
        | PhaseZeroArtifactState::GeneratedStale
        | PhaseZeroArtifactState::Ambiguous => {
            Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero {
                version: version.to_string(),
            }))
        }
    }
}

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
    let checksum_up = compute_committed_sql_checksum(&up_sql, ResetSqlSide::Up);
    // Try to read the down file too — the version's down checksum is
    // best-effort. Missing down is fine; record a None so the row
    // still inserts.
    let down_path = up_path
        .parent()
        .map(|p| p.join(super::naming::down_filename(version)));
    let checksum_down = down_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|down_sql| compute_committed_down_sql_checksum(&down_sql));
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
        leaf_identity: None,
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
/// **Single-bucket scope.** The squash predicate `versions >= from` is
/// applied to EXACTLY ONE bucket — the bucket where `from` actually
/// exists. The squash contract:
/// - If `app_filter` is `Some(label)`, the squash is scoped to
///   `(active_db, label)`; if `from` is absent there it errors with
///   [`AttuneRefusal::SquashFromVersionNotFound`].
/// - If `app_filter` is `None` and `from` exists in exactly one bucket,
///   that bucket is the target.
/// - If `app_filter` is `None` and `from` exists in multiple buckets,
///   we refuse with [`AttuneRefusal::SquashFromVersionAmbiguous`] and
///   require the operator to disambiguate.
/// - If `from` exists in zero buckets, we refuse with
///   [`AttuneRefusal::SquashFromVersionNotFound`].
///   **Retained-row checksum refresh.** After writing the squashed
///   SQL file, we recompute its `up` (and best-effort `down`) checksum
///   and `UPDATE` the retained `from` ledger row's `checksum_up`,
///   `checksum_down`, and `description` to match. The pre-
///   implementation left the retained row's checksum describing the
///   PRE-squash file content — every subsequent verify or apply path
///   would surface drift authored by squash itself.
#[allow(clippy::too_many_arguments)]
async fn run_squash(
    ctx: &mut DjogiContext,
    workspace_root: &Path,
    from: &str,
    publish: bool,
    app_filter: Option<&str>,
    disk: &BTreeMap<BucketKey, BTreeMap<String, PathBuf>>,
    report: &mut AttuneReport,
    diff_entries: Vec<AttuneEntry>,
) -> Result<(), AttuneError> {
    let mut entries = diff_entries;

    let bucket = locate_squash_target(disk, from, app_filter)?.clone();
    let versions = disk.get(&bucket).expect("target_bucket came from disk map");

    // Stale Phase 0 guard: classify the retained file before any squash
    // mutation (retained rewrite, down rewrite, sidecar delete, subsumed
    // delete, ledger update, parent pointer, publish).
    if let Some(from_path) = versions.get(&from.to_string()) {
        check_phase_zero_attune_entry(from_path, from)?;
    }

    // Lexical compare = chronological because version_prefix is `V<14 digits>`.
    let to_squash: Vec<(&String, &PathBuf)> = versions
        .iter()
        .filter(|(v, _)| v.as_str() >= from)
        .collect();
    if to_squash.len() <= 1 {
        entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        report.entries = entries;
        return Ok(());
    }

    let dir = bucket_dir(workspace_root, &bucket);

    let (combined_up, combined_down_segments) = build_combined_sql(&dir, &to_squash)?;

    // Reverse-order concat so a rollback unwinds in the same order
    // apply happened.
    let mut combined_down = String::new();
    for seg in combined_down_segments.iter().rev() {
        combined_down.push_str(seg);
        combined_down.push('\n');
    }
    let wrote_down = !combined_down.is_empty();

    let new_up_path = dir.join(up_filename(from));
    std::fs::write(&new_up_path, combined_up.as_bytes()).map_err(|e| {
        AttuneError::SqlWriteFailed {
            path: new_up_path.clone(),
            source: e,
        }
    })?;
    if wrote_down {
        let new_down_path = dir.join(down_filename(from));
        std::fs::write(&new_down_path, combined_down.as_bytes()).map_err(|e| {
            AttuneError::SqlWriteFailed {
                path: new_down_path.clone(),
                source: e,
            }
        })?;
    }
    delete_replay_plan_sidecar_if_exists(&dir, from)?;

    delete_subsumed(ctx, &dir, &to_squash, from, &bucket, &mut entries).await?;
    refresh_retained_row(
        ctx,
        from,
        &bucket,
        &combined_up,
        if wrote_down {
            Some(combined_down.as_str())
        } else {
            None
        },
    )
    .await?;

    report.mutated = true;
    report.squashed_to = Some(from.to_string());

    // When `publish` is set, the publisher commits every disk change
    // made above and pushes to `origin` so a single `attune --publish`
    // invocation lands the rewrite in the remote.
    if publish && report.mutated {
        run_git_commit_and_publish(workspace_root, from)?;
        report.published = true;
    }

    entries.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    report.entries = entries;
    Ok(())
}

/// Resolve the unique bucket containing `from`. Errors with
/// [`AttuneRefusal::SquashFromVersionNotFound`] when no bucket has
/// `from`, and [`AttuneRefusal::SquashFromVersionAmbiguous`] when
/// more than one bucket does (after applying `app_filter` if provided).
fn locate_squash_target<'a>(
    disk: &'a BTreeMap<BucketKey, BTreeMap<String, PathBuf>>,
    from: &str,
    app_filter: Option<&str>,
) -> Result<&'a BucketKey, AttuneError> {
    let mut matching: Vec<&BucketKey> = Vec::new();
    for (bucket, versions) in disk {
        if let Some(label) = app_filter
            && bucket.app.as_str() != label
        {
            continue;
        }
        if versions.contains_key(from) {
            matching.push(bucket);
        }
    }
    match matching.len() {
        0 => Err(AttuneError::Refused(
            AttuneRefusal::SquashFromVersionNotFound {
                version: from.to_string(),
            },
        )),
        1 => Ok(matching[0]),
        _ => {
            let mut buckets_rendered: Vec<String> = matching
                .iter()
                .map(|b| {
                    let app_render = if b.app.is_empty() { "_global_" } else { &b.app };
                    format!("{}/{}", b.database, app_render)
                })
                .collect();
            buckets_rendered.sort();
            Err(AttuneError::Refused(
                AttuneRefusal::SquashFromVersionAmbiguous {
                    version: from.to_string(),
                    buckets: buckets_rendered,
                },
            ))
        }
    }
}

fn delete_replay_plan_sidecar_if_exists(dir: &Path, version: &str) -> Result<(), AttuneError> {
    let path = dir.join(committed_replay_plan_filename(version));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(AttuneError::SqlDeleteFailed { path, source: e }),
    }
}

/// Read every up file in `to_squash`, concatenate the bodies wrapped
/// with `-- begin <version>` / `-- end <version>` markers, and collect
/// the matching per-version down-side segments (best-effort: missing
/// down files are silently skipped). The caller reverses the down
/// segments before writing.
fn build_combined_sql(
    dir: &Path,
    to_squash: &[(&String, &PathBuf)],
) -> Result<(String, Vec<String>), AttuneError> {
    let mut combined_up = String::new();
    let mut combined_down_segments: Vec<String> = Vec::new();
    for (version, path) in to_squash {
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
        let down_path = dir.join(down_filename(version));
        if let Ok(down_sql) = std::fs::read_to_string(&down_path) {
            combined_down_segments.push(format!(
                "-- begin {version} (reverse)\n{down_sql}\n-- end {version}\n",
            ));
        }
    }
    Ok((combined_up, combined_down_segments))
}

/// Delete every up/down file in `to_squash` except the retained `from`
/// version, drop the matching ledger rows, and append a `Squashed`
/// entry per deleted version.
async fn delete_subsumed(
    ctx: &mut DjogiContext,
    dir: &Path,
    to_squash: &[(&String, &PathBuf)],
    from: &str,
    bucket: &BucketKey,
    entries: &mut Vec<AttuneEntry>,
) -> Result<(), AttuneError> {
    for (version, path) in to_squash {
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
        delete_replay_plan_sidecar_if_exists(dir, version)?;
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
    Ok(())
}

/// Refresh the retained `from` ledger row's checksum + description so it
/// describes the post-squash file content, not the pre-squash file
/// content. Without this, every subsequent verify or apply path would
/// surface drift authored by squash itself.
/// `combined_down` is `None` when no per-version down files existed or the
/// retained down file is comment-only; the row's `checksum_down` is set to
/// `NULL` in that case.
async fn refresh_retained_row(
    ctx: &mut DjogiContext,
    from: &str,
    bucket: &BucketKey,
    combined_up: &str,
    combined_down: Option<&str>,
) -> Result<(), AttuneError> {
    let new_checksum_up = compute_checksum([combined_up]);
    let new_checksum_down = combined_down.and_then(compute_committed_down_sql_checksum);
    let prior_description: Option<String> = ctx
        .query_one(
            "SELECT description FROM djogi_schema_migrations \
             WHERE version = $1 AND app_label = $2",
            &[&from.to_string(), &bucket.app],
        )
        .await
        .ok()
        .and_then(|row| row.try_get::<_, String>(0).ok());
    let new_description = match prior_description {
        Some(prev) => format!("squashed migration starting at {from} (was: {prev})"),
        // No retained ledger row yet — squash also runs on workspaces
        // whose `from` was never recorded. The UPDATE below is a no-op
        // in that case; we still produce a description for parity with
        // the recorded path.
        None => format!("squashed migration starting at {from}"),
    };
    ctx.execute(
        "UPDATE djogi_schema_migrations \
         SET checksum_up = $1, checksum_down = $2, description = $3 \
         WHERE version = $4 AND app_label = $5",
        &[
            &new_checksum_up,
            &new_checksum_down,
            &new_description,
            &from.to_string(),
            &bucket.app,
        ],
    )
    .await
    .map_err(|e| AttuneError::LedgerQueryFailed { source: e })?;
    Ok(())
}

/// Stage every change in the migrations working tree, commit it with a
/// canonical squash message, then push the current branch to `origin`.
/// The publisher's contract is "commit the squash mutation, then push to
/// origin" — the operator does not run `git add` / `git commit` themselves
/// between the squash mutation and the push.
/// Each step captures stderr so an operator-facing diagnostic carries the
/// precise failure point. A failure anywhere in the chain surfaces as
/// [`AttuneError::GitPublishFailed`] — the stderr prefix names the step
/// that failed (`git add`, `git commit`, `git push`) so the operator can
/// correct without re-running.
fn run_git_commit_and_publish(workspace_root: &Path, from: &str) -> Result<(), AttuneError> {
    let migrations_root = super::target::migrations_root(workspace_root);
    let commit_msg = format!("djogi attune --squash from {from}");

    // Idempotency-recovery probe: if a previous `--publish` attempt
    // landed the commit but failed at the push step, the working tree
    // is now clean (originals already deleted, squash file already
    // committed) — a fresh `attune --squash` would silently no-op
    // because no version >= `from` remains on disk. Detect that
    // shape by looking at HEAD's commit subject; if it already
    // matches our canonical squash message for the same `from`, skip
    // straight to push so the recovery path is `attune --squash --publish`
    // typed verbatim, not `cd migrations && git push origin HEAD`.
    let head_subject_out = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("log")
        .arg("-1")
        .arg("--pretty=%s")
        .output()
        .map_err(|e| AttuneError::GitPublishFailed {
            stderr: format!("failed to spawn `git log`: {e}"),
            status_code: None,
        })?;
    let head_subject = String::from_utf8_lossy(head_subject_out.stderr_or_stdout())
        .trim()
        .to_string();
    let commit_already_landed = head_subject_out.status.success() && head_subject == commit_msg;

    if !commit_already_landed {
        // Step 1: stage every mutation produced by run_squash above. We
        // pass `-A` so deletions of subsumed SQL files are picked up
        // alongside the squashed file's write.
        let add_out = std::process::Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("add")
            .arg("-A")
            .output()
            .map_err(|e| AttuneError::GitPublishFailed {
                stderr: format!("failed to spawn `git add`: {e}"),
                status_code: None,
            })?;
        if !add_out.status.success() {
            let stderr = String::from_utf8_lossy(&add_out.stderr).into_owned();
            return Err(AttuneError::GitPublishFailed {
                stderr: format!("`git add -A` failed: {stderr}"),
                status_code: add_out.status.code(),
            });
        }

        // Step 2: commit the staged squash. The message names the
        // canonical `from` version so the audit trail in the migrations
        // submodule's history points back to the operator's intent.
        let commit_out = std::process::Command::new("git")
            .arg("-C")
            .arg(&migrations_root)
            .arg("commit")
            .arg("-m")
            .arg(&commit_msg)
            .output()
            .map_err(|e| AttuneError::GitPublishFailed {
                stderr: format!("failed to spawn `git commit`: {e}"),
                status_code: None,
            })?;
        if !commit_out.status.success() {
            let stderr = String::from_utf8_lossy(&commit_out.stderr).into_owned();
            return Err(AttuneError::GitPublishFailed {
                stderr: format!("`git commit` failed: {stderr}"),
                status_code: commit_out.status.code(),
            });
        }
    }

    // Step 3: pre-check that `origin` is configured so the operator
    // gets a clear "no remote" error instead of the cryptic git-push
    // "fatal: 'origin' does not appear to be a git repository".
    let remote_check = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("config")
        .arg("--get")
        .arg("remote.origin.url")
        .output()
        .map_err(|e| AttuneError::GitPublishFailed {
            stderr: format!("failed to probe remote.origin.url: {e}"),
            status_code: None,
        })?;
    if !remote_check.status.success() {
        return Err(AttuneError::GitPublishFailed {
            stderr: "remote `origin` is not configured on the migrations submodule; \
                     set a remote with `git -C migrations remote add origin <url>` \
                     before retrying `--publish`"
                .to_string(),
            status_code: remote_check.status.code(),
        });
    }

    // Step 3b: push the current branch to `origin`. We rely on the
    // operator having a tracking branch already configured (squash is
    // dev-only and the migrations repo is expected to have its
    // upstream wired up at clone time). If push fails after the
    // commit landed locally, re-running `attune --squash --publish`
    // will skip the no-op stage/commit (via the head_subject probe
    // above) and retry just the push.
    let push_out = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("push")
        .arg("origin")
        .arg("HEAD")
        .output()
        .map_err(|e| AttuneError::GitPublishFailed {
            stderr: format!("failed to spawn `git push`: {e}"),
            status_code: None,
        })?;
    if !push_out.status.success() {
        let stderr = String::from_utf8_lossy(&push_out.stderr).into_owned();
        return Err(AttuneError::GitPublishFailed {
            stderr: format!(
                "`git push origin HEAD` failed: {stderr}\n\
                 Hint: the squash commit IS on the local migrations branch; \
                 retry with `attune --squash --publish` once the push issue \
                 is resolved (auth, network, etc.)."
            ),
            status_code: push_out.status.code(),
        });
    }
    Ok(())
}

/// Helper trait to read either stdout (success path) or stderr (failure
/// path) from a `std::process::Output` without an extra `match`.
trait OutputReadStdoutOrStderr {
    fn stderr_or_stdout(&self) -> &[u8];
}

impl OutputReadStdoutOrStderr for std::process::Output {
    fn stderr_or_stdout(&self) -> &[u8] {
        if self.status.success() {
            &self.stdout
        } else {
            &self.stderr
        }
    }
}

// ── Target resolution + parent pointer write ─────────────────────────────

/// Resolve a Git target string against the migrations submodule.
/// Tries `git -C <migrations_root> rev-parse <target>^{commit}` first;
/// on failure falls back to `git fetch --all` then retries the
/// rev-parse. The `^{commit}` peel ensures the result is a concrete
/// commit SHA — operators who pass tag names get the SHA the tag
/// points at, not the tag object's SHA.
/// Returns the lowercase 40-char SHA on success.
/// **No regex.** The target string is passed verbatim to git; we
/// never parse it. The captured stdout is trimmed only.
fn resolve_git_target(workspace_root: &Path, target: &str) -> Result<String, AttuneError> {
    let migrations_root = super::target::migrations_root(workspace_root);

    // Local rev-parse first. The `^{commit}` peel is critical
    // `git rev-parse v1.2.3` on a tag returns the tag-object SHA, but
    // `git rev-parse v1.2.3^{commit}` returns the underlying commit
    // SHA, which is what the parent submodule pointer needs.
    let local_arg = format!("{target}^{{commit}}");
    let local = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("rev-parse")
        .arg("--verify")
        .arg(&local_arg)
        .output()
        .map_err(|e| AttuneError::GitTargetResolveFailed {
            target: target.to_string(),
            stderr: format!("failed to spawn `git rev-parse`: {e}"),
            status_code: None,
        })?;
    if local.status.success() {
        return Ok(String::from_utf8_lossy(&local.stdout).trim().to_string());
    }

    // Local resolution failed. Try a remote fetch, then retry.
    let fetch = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("fetch")
        .arg("--all")
        .arg("--tags")
        .output()
        .map_err(|e| AttuneError::GitFetchFailed {
            stderr: format!("failed to spawn `git fetch`: {e}"),
            status_code: None,
        })?;
    if !fetch.status.success() {
        // Distinguish "no remote configured" (an arguably-clean state
        // the operator may want a local-only attune) from "fetch
        // tried and failed" (network / auth / ref-spec problem).
        // `git fetch --all` exits 0 when no remotes exist, so any
        // non-zero status here is a real fetch failure.
        return Err(AttuneError::GitFetchFailed {
            stderr: String::from_utf8_lossy(&fetch.stderr).into_owned(),
            status_code: fetch.status.code(),
        });
    }

    // Retry the local rev-parse against the now-updated refs.
    let retry = std::process::Command::new("git")
        .arg("-C")
        .arg(&migrations_root)
        .arg("rev-parse")
        .arg("--verify")
        .arg(&local_arg)
        .output()
        .map_err(|e| AttuneError::GitTargetResolveFailed {
            target: target.to_string(),
            stderr: format!("failed to spawn `git rev-parse` (retry): {e}"),
            status_code: None,
        })?;
    if retry.status.success() {
        return Ok(String::from_utf8_lossy(&retry.stdout).trim().to_string());
    }
    Err(AttuneError::GitTargetResolveFailed {
        target: target.to_string(),
        stderr: String::from_utf8_lossy(&retry.stderr).into_owned(),
        status_code: retry.status.code(),
    })
}

/// Update the parent repo's recorded submodule pointer to `new_sha`.
/// Walks `git update-index --cacheinfo 160000,<sha>,<path>` against
/// the parent repo (the workspace root) so the next parent-side
/// `git diff --staged` shows the submodule pointer move. The 160000
/// mode is git's gitlink-mode marker for submodules; the path is
/// `migrations` (the submodule directory name on disk).
/// **No regex.** The new SHA is interpolated as-is into the cacheinfo
/// argument; nothing else is parsed.
/// **Failure modes.** The most common is the parent repo having
/// uncommitted staged changes to the submodule path that conflict
/// with the intended pointer write. The captured stderr surfaces the
/// precise reason verbatim.
fn update_parent_submodule_pointer(
    workspace_root: &Path,
    new_sha: &str,
) -> Result<(), AttuneError> {
    let cacheinfo = format!("160000,{new_sha},migrations");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .arg("update-index")
        .arg("--cacheinfo")
        .arg(&cacheinfo)
        .output()
        .map_err(|e| AttuneError::GitUpdateSubmodulePointerFailed {
            new_sha: new_sha.to_string(),
            stderr: format!("failed to spawn `git update-index`: {e}"),
            status_code: None,
        })?;
    if !out.status.success() {
        return Err(AttuneError::GitUpdateSubmodulePointerFailed {
            new_sha: new_sha.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status_code: out.status.code(),
        });
    }
    Ok(())
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
    use std::collections::BTreeSet;
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

    fn current_production_phase_zero_sql() -> String {
        super::super::bootstrap::compose_phase_zero(
            "main",
            &BTreeSet::new(),
            super::super::bootstrap::DEFAULT_NODE_ID,
            false,
        )
        .expect("compose production Phase 0")
    }

    fn markerless_seed_phase_zero_sql() -> String {
        phase_zero_with_seed_statement("INSERT INTO heer.heer_nodes (id) VALUES (1);")
    }

    fn phase_zero_with_seed_statement(statement: &str) -> String {
        let mut sql = current_production_phase_zero_sql();
        sql.push('\n');
        sql.push_str(statement);
        sql.push('\n');
        sql
    }

    fn extended_seed_statement_cases() -> [(&'static str, &'static str); 4] {
        [
            (
                "cte_insert",
                "WITH rows AS (SELECT 1) INSERT INTO heer.heer_nodes (id) VALUES (1);",
            ),
            (
                "cte_delete",
                "WITH moved AS (DELETE FROM heer.heer_node_state RETURNING *) SELECT 1;",
            ),
            (
                "merge",
                "MERGE INTO heer.heer_nodes AS target USING incoming ON false WHEN NOT MATCHED THEN INSERT (id) VALUES (1);",
            ),
            (
                "copy_from",
                "COPY \"heer\".\"heer_ranj_node_state\" (\"node_id\") FROM STDIN;",
            ),
        ]
    }

    fn current_single_node_dev_phase_zero_sql() -> String {
        super::super::bootstrap::compose_phase_zero("main", &BTreeSet::new(), 1, true)
            .expect("compose single-node-dev Phase 0")
    }

    fn legacy_seeded_banner(sql: &str) -> String {
        sql.replace(
            "Djogi bootstrap migration — HeeRanjID + extensions + node seed",
            "Djogi Phase 0 bootstrap — HeeRanjID + extensions + node seed",
        )
    }

    fn generated_stale_phase_zero_sql() -> String {
        let mut sql = current_single_node_dev_phase_zero_sql();
        let start = sql.find("DO $djogi$").expect("dynamic defaults block");
        let end = sql[start..]
            .find("SET heer.node_id = '1';")
            .map(|offset| start + offset)
            .expect("session SET after dynamic defaults");
        sql.replace_range(
            start..end,
            "ALTER DATABASE \"main\" SET heer.node_id = '1';\n\
             ALTER DATABASE \"main\" SET heer.ranj_node_id = '1';\n",
        );
        sql
    }

    fn legacy_generated_stale_phase_zero_sql() -> String {
        legacy_seeded_banner(&generated_stale_phase_zero_sql())
    }

    fn incomplete_phase_zero_sql() -> String {
        current_production_phase_zero_sql()
            .replace("-- HeeRanjID base schema + functions (idempotent).\n", "")
    }

    #[test]
    fn scan_disk_picks_up_only_up_files() {
        let root = temp_root("scan_disk_up_only");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        // Up file.
        fs::write(
            dir.join("V20260425010203__init.sdjql"),
            "CREATE TABLE foo();",
        )
        .unwrap();
        // Down file.
        fs::write(
            dir.join("V20260425010203__init.down.sdjql"),
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
    fn squash_sidecar_cleanup_removes_committed_replay_plan() {
        let root = temp_root("squash_sidecar_cleanup");
        let version = "V20260425010203__init";
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join(committed_replay_plan_filename(version));
        fs::write(&sidecar, "{}").unwrap();

        delete_replay_plan_sidecar_if_exists(&dir, version).expect("delete sidecar");
        assert!(
            !sidecar.exists(),
            "squash must not leave stale replay manifests beside rewritten SQL"
        );
        delete_replay_plan_sidecar_if_exists(&dir, version).expect("missing sidecar is ok");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_disk_groups_by_bucket() {
        let root = temp_root("scan_disk_buckets");
        fs::create_dir_all(root.join("migrations/main/billing")).unwrap();
        fs::create_dir_all(root.join("migrations/main/_global_")).unwrap();
        fs::write(
            root.join("migrations/main/billing/V20260101000001__init.sdjql"),
            "",
        )
        .unwrap();
        fs::write(
            root.join("migrations/main/_global_/V20260101000002__init.sdjql"),
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

    // ── --squash gate trio ───────────────────────────────────────────────

    /// Gate refusals render an operator-actionable message naming the
    /// field/env-var the operator must adjust to opt-in.
    #[test]
    fn u2_squash_dev_mode_off_message_names_field() {
        let r = AttuneRefusal::SquashDevModeOff;
        let s = format!("{r}");
        assert!(s.contains("dev_mode"), "must name the dev_mode field: {s}");
        assert!(
            s.contains("Djogi.toml") || s.contains("[database]"),
            "must hint at the config location: {s}"
        );
    }

    #[test]
    fn u2_squash_env_is_production_message_names_env_value() {
        let r = AttuneRefusal::SquashEnvIsProduction {
            env_value: "production".to_string(),
        };
        let s = format!("{r}");
        assert!(s.contains("DJOGI_ENV"), "must name the env-var: {s}");
        assert!(s.contains("production"), "must echo the value: {s}");
    }

    // ── Dry-run diagnostics + git target shape ───────────────────────────

    /// Dry-run diagnostic codes are stable across runs so operator-facing
    /// tooling can branch on them without parsing the message body.
    #[test]
    fn u1_dry_run_diagnostic_codes_are_stable() {
        assert_eq!(
            AttuneDiagnostic::DryRunMutationsSkipped { mode: "Record" }.code(),
            "ATTUNE-002"
        );
        assert_eq!(
            AttuneDiagnostic::DryRunRecordSkipped {
                resolved_target: None,
            }
            .code(),
            "ATTUNE-003"
        );
        assert_eq!(
            AttuneDiagnostic::DryRunSquashRecordSkipped {
                resolved_target: None,
            }
            .code(),
            "ATTUNE-003"
        );
    }

    /// `DryRunMutationsSkipped` mentions the mode + the `--apply`
    /// remediation; the dry-run record diagnostics mention the
    /// resolved SHA when present and fall back to accurate guidance
    /// when absent.
    #[test]
    fn u1_dry_run_diagnostic_messages_are_actionable() {
        let d = AttuneDiagnostic::DryRunMutationsSkipped { mode: "Squash" };
        let s = d.to_string();
        assert!(s.contains("Squash"), "must name mode: {s}");
        assert!(s.contains("--apply"), "must hint at remediation: {s}");

        let d_with = AttuneDiagnostic::DryRunRecordSkipped {
            resolved_target: Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()),
        };
        let s_with = d_with.to_string();
        assert!(
            s_with.contains("deadbeef"),
            "must echo resolved SHA: {s_with}"
        );
        assert!(s_with.contains("--apply"));
        assert!(
            s_with.contains("parent submodule pointer would be updated to"),
            "must describe skipped parent-pointer update: {s_with}"
        );
        assert!(
            s_with.contains("`--record` was requested without `--apply`"),
            "explicit record case must name the causal flag: {s_with}"
        );

        let d_none = AttuneDiagnostic::DryRunRecordSkipped {
            resolved_target: None,
        };
        let s_none = d_none.to_string();
        assert!(
            s_none.contains("no parent submodule pointer would be updated"),
            "must describe the unresolved-target outcome accurately: {s_none}"
        );
        assert!(s_none.contains("--target <ref>"));
        assert!(
            s_none.contains("`--record` was requested without `--apply`"),
            "explicit record case must still name the causal flag when unresolved: {s_none}"
        );
    }

    /// Lock the unresolved-target prose for `DryRunSquashRecordSkipped`
    /// too, so the squash-implied None path is held to the same
    /// coverage bar as the explicit path.
    #[test]
    fn dry_run_squash_record_skipped_none_wording_is_neutral() {
        let s = AttuneDiagnostic::DryRunSquashRecordSkipped {
            resolved_target: None,
        }
        .to_string();
        assert!(
            s.contains("no parent submodule pointer would be updated"),
            "must describe the unresolved-target outcome accurately: {s}"
        );
        assert!(s.contains("--target <ref>"));
        assert!(s.contains("--apply"));
        assert!(
            !s.contains("`--record`"),
            "squash-implied wording must NOT name --record: {s}"
        );
    }

    /// The direct `--record` path names the causal flag, while the
    /// squash-implied path stays neutral.
    #[test]
    fn u9_record_source_drives_diagnostic_wording() {
        let explicit = AttuneDiagnostic::DryRunRecordSkipped {
            resolved_target: Some("abcd".to_string()),
        }
        .to_string();
        let squash = AttuneDiagnostic::DryRunSquashRecordSkipped {
            resolved_target: Some("abcd".to_string()),
        }
        .to_string();
        assert!(
            explicit.contains(
                "`--record` was requested without `--apply`; parent submodule pointer \
                 would be updated to `abcd`"
            ),
            "direct --record wording must name the explicit flag: {explicit}"
        );
        assert!(
            squash.contains(
                "would update parent submodule pointer to `abcd` but `--apply` was not \
                 provided; no parent index mutation happened"
            ),
            "squash-implied wording must match neutral prose: {squash}"
        );
        assert!(
            explicit.contains("--record"),
            "direct --record wording must mention the explicit flag: {explicit}"
        );
        assert!(
            !squash.contains("--record requested"),
            "squash-implied wording must NOT say --record requested: {squash}"
        );
    }

    #[test]
    fn attune_diagnostic_dry_run_record_skipped_api_stability() {
        let diagnostic = AttuneDiagnostic::DryRunRecordSkipped {
            resolved_target: Some("abc123".to_string()),
        };
        let resolved_target = match diagnostic.clone() {
            AttuneDiagnostic::DryRunRecordSkipped { resolved_target } => resolved_target,
            AttuneDiagnostic::LedgerTableMissing { .. }
            | AttuneDiagnostic::DryRunMutationsSkipped { .. }
            | AttuneDiagnostic::DryRunSquashRecordSkipped { .. } => {
                panic!("expected DryRunRecordSkipped")
            }
        };
        assert_eq!(resolved_target.as_deref(), Some("abc123"));
        assert_eq!(
            diagnostic.to_string(),
            "[ATTUNE-003] `--record` was requested without `--apply`; parent submodule \
             pointer would be updated to `abc123` but no parent index mutation happened \
             — re-run with `--apply` to commit"
        );
    }

    /// New `AttuneError` variants render operator-actionable messages
    /// naming the failed step + status code (when available).
    #[test]
    fn u1_git_target_resolve_failed_message_names_target() {
        let e = AttuneError::GitTargetResolveFailed {
            target: "feature/branch".to_string(),
            stderr: "fatal: ambiguous argument".to_string(),
            status_code: Some(128),
        };
        let s = e.to_string();
        assert!(s.contains("feature/branch"));
        assert!(s.contains("128"));
        assert!(s.contains("rev-parse"));
    }

    #[test]
    fn u1_git_update_submodule_pointer_failed_message_names_sha() {
        let e = AttuneError::GitUpdateSubmodulePointerFailed {
            new_sha: "deadbeef".to_string(),
            stderr: "fatal: refusing to overwrite".to_string(),
            status_code: Some(128),
        };
        let s = e.to_string();
        assert!(s.contains("deadbeef"));
        assert!(s.contains("--record"));
        assert!(s.contains("submodule pointer"));
    }

    /// `djogi_env_is_production` returns `Some(value)` only for a
    /// case-insensitive `"production"` byte sequence; everything else
    /// resolves to `None`. Walks both happy and adversarial inputs to
    /// pin the exact ASCII case-insensitive contract — without
    /// allocating (no `to_lowercase`) and without regex.
    #[test]
    fn u2_djogi_env_is_production_predicate_matches_only_exact_word() {
        // Save / restore the env var so the test does not leak state.
        // Tests run with `--test-threads=1` per the project's
        // pre-commit policy so concurrent env mutation is not a
        // concern in this configuration.
        let prior = std::env::var("DJOGI_ENV").ok();

        // Unset → None.
        // SAFETY: serial test execution; no other thread reads DJOGI_ENV.
        unsafe { std::env::remove_var("DJOGI_ENV") };
        assert_eq!(djogi_env_is_production(), None);

        // Exact match (lowercase) → Some(value).
        unsafe { std::env::set_var("DJOGI_ENV", "production") };
        assert_eq!(djogi_env_is_production(), Some("production".to_string()));

        // Mixed case still matches (case-insensitive ASCII compare).
        unsafe { std::env::set_var("DJOGI_ENV", "Production") };
        assert_eq!(djogi_env_is_production(), Some("Production".to_string()));
        unsafe { std::env::set_var("DJOGI_ENV", "PRODUCTION") };
        assert_eq!(djogi_env_is_production(), Some("PRODUCTION".to_string()));

        // Length mismatch → None (prefix / suffix do not match).
        unsafe { std::env::set_var("DJOGI_ENV", "prod") };
        assert_eq!(djogi_env_is_production(), None);
        unsafe { std::env::set_var("DJOGI_ENV", "productionx") };
        assert_eq!(djogi_env_is_production(), None);

        // Unrelated values → None.
        unsafe { std::env::set_var("DJOGI_ENV", "development") };
        assert_eq!(djogi_env_is_production(), None);
        unsafe { std::env::set_var("DJOGI_ENV", "staging") };
        assert_eq!(djogi_env_is_production(), None);
        unsafe { std::env::set_var("DJOGI_ENV", "") };
        assert_eq!(djogi_env_is_production(), None);

        // Restore.
        match prior {
            Some(v) => unsafe { std::env::set_var("DJOGI_ENV", v) },
            None => unsafe { std::env::remove_var("DJOGI_ENV") },
        }
    }

    // ── Stale Phase 0 attune guards ────────────────────────────────────

    /// Helper to generate a generated-stale Phase 0 file for testing.
    fn write_generated_stale_phase_zero(dir: &Path) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            generated_stale_phase_zero_sql(),
        )
        .unwrap();
    }

    fn write_legacy_generated_stale_phase_zero(dir: &Path) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            legacy_generated_stale_phase_zero_sql(),
        )
        .unwrap();
    }

    /// Helper to generate an identity-free production Phase 0 file for testing.
    fn write_current_production_phase_zero(dir: &Path) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            current_production_phase_zero_sql(),
        )
        .unwrap();
    }

    fn write_seed_capable_phase_zero(dir: &Path) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            current_single_node_dev_phase_zero_sql(),
        )
        .unwrap();
    }

    fn write_markerless_seed_phase_zero(dir: &Path) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            markerless_seed_phase_zero_sql(),
        )
        .unwrap();
    }

    fn write_phase_zero_with_seed_statement(dir: &Path, statement: &str) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            phase_zero_with_seed_statement(statement),
        )
        .unwrap();
    }

    fn write_missing_phase_zero(dir: &Path) {
        fs::write(dir.join(up_filename(PHASE_ZERO_VERSION)), " \n\t ").unwrap();
    }

    fn write_incomplete_phase_zero(dir: &Path) {
        fs::write(
            dir.join(up_filename(PHASE_ZERO_VERSION)),
            incomplete_phase_zero_sql(),
        )
        .unwrap();
    }

    /// Stale Phase 0 attune Record refusal: generated-stale file should be
    /// refused before ledger INSERT. This test exercises the guard directly
    /// via check_phase_zero_attune_entry.
    #[test]
    fn record_refuses_generated_stale_phase_zero() {
        let root = temp_root("record_stale_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_generated_stale_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_err(),
            "Record must refuse generated-stale Phase 0; got Ok"
        );
        if let Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { version })) = result {
            assert_eq!(version, PHASE_ZERO_VERSION);
        } else {
            panic!("Expected StalePhaseZero refusal, got: {:?}", result);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_refuses_legacy_generated_stale_phase_zero() {
        let root = temp_root("record_legacy_stale_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_legacy_generated_stale_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_err(),
            "Record must refuse legacy generated-stale Phase 0; got Ok"
        );
        if let Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { version })) = result {
            assert_eq!(version, PHASE_ZERO_VERSION);
        } else {
            panic!("Expected StalePhaseZero refusal, got: {:?}", result);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_refuses_seed_capable_phase_zero() {
        let root = temp_root("record_seed_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_seed_capable_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            matches!(
                result,
                Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { .. }))
            ),
            "Record must refuse seed-capable Phase 0; got {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_refuses_markerless_seed_phase_zero() {
        let root = temp_root("record_markerless_seed_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_markerless_seed_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            matches!(
                result,
                Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { .. }))
            ),
            "Record must refuse markerless seed Phase 0; got {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_refuses_extended_seed_phase_zero() {
        for (name, statement) in extended_seed_statement_cases() {
            let root = temp_root(&format!("record_extended_seed_p0_{name}"));
            let dir = root.join("migrations/main/billing");
            fs::create_dir_all(&dir).unwrap();
            write_phase_zero_with_seed_statement(&dir, statement);

            let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
            let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
            assert!(
                matches!(
                    result,
                    Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { .. }))
                ),
                "Record must refuse extended seed Phase 0 {name}; got {result:?}"
            );

            let _ = fs::remove_dir_all(&root);
        }
    }

    /// Identity-free production Phase 0 attune Record passes.
    #[test]
    fn record_accepts_identity_free_production_phase_zero() {
        let root = temp_root("record_current_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_current_production_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_ok(),
            "Record must accept identity-free Phase 0; got Err: {:?}",
            result
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn attune_does_not_broaden_missing_phase_zero_into_stale_refusal() {
        let root = temp_root("attune_missing_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_missing_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_ok(),
            "attune must not broaden missing Phase 0 artifacts into stale refusal; got {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn attune_does_not_broaden_incomplete_phase_zero_into_stale_refusal() {
        let root = temp_root("attune_incomplete_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_incomplete_phase_zero(&dir);

        let up_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&up_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_ok(),
            "attune must not broaden incomplete Phase 0 artifacts into stale refusal; got {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn phase_zero_attune_preflight_is_record_squash_apply_only() {
        assert!(
            !requires_phase_zero_attune_preflight(&AttuneMode::DiffOnly, false),
            "DiffOnly dry-run must not run stale Phase 0 preflight"
        );
        assert!(
            !requires_phase_zero_attune_preflight(&AttuneMode::DiffOnly, true),
            "DiffOnly with --apply/--record may update a pointer but must not preflight Phase 0 SQL"
        );
        assert!(
            !requires_phase_zero_attune_preflight(
                &AttuneMode::Record {
                    reason: "out-of-band".to_string()
                },
                false
            ),
            "Record dry-run must report diff without stale Phase 0 refusal"
        );
        assert!(
            !requires_phase_zero_attune_preflight(
                &AttuneMode::Squash {
                    from: PHASE_ZERO_VERSION.to_string(),
                    publish: false,
                    app: None
                },
                false
            ),
            "Squash dry-run must report diff without stale Phase 0 refusal"
        );
        assert!(
            requires_phase_zero_attune_preflight(
                &AttuneMode::Record {
                    reason: "out-of-band".to_string()
                },
                true
            ),
            "Record --apply must preflight stale Phase 0 before ledger mutation"
        );
        assert!(
            requires_phase_zero_attune_preflight(
                &AttuneMode::Squash {
                    from: PHASE_ZERO_VERSION.to_string(),
                    publish: false,
                    app: None
                },
                true
            ),
            "Squash --apply must preflight stale Phase 0 before file/ledger mutation"
        );
    }

    /// Non-Phase 0 files should always pass the attune Phase 0 guard.
    #[test]
    fn record_skips_phase_zero_check_for_non_phase_zero_version() {
        let root = temp_root("record_non_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("V20260101000001__init.sdjql"),
            "CREATE TABLE foo();",
        )
        .unwrap();

        let up_path = dir.join("V20260101000001__init.sdjql");
        let result = check_phase_zero_attune_entry(&up_path, "V20260101000001__init");
        assert!(
            result.is_ok(),
            "Non-Phase 0 files should pass the guard; got Err: {:?}",
            result
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Stale Phase 0 attune Squash refusal: generated-stale file as
    /// squash target should be refused before retained rewrite.
    #[test]
    fn squash_refuses_generated_stale_phase_zero_as_from() {
        let root = temp_root("squash_stale_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_generated_stale_phase_zero(&dir);

        // Add a later migration so squash would have multiple files.
        fs::write(
            dir.join("V20260101000001__init.sdjql"),
            "CREATE TABLE bar();",
        )
        .unwrap();

        let from_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&from_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_err(),
            "Squash must refuse generated-stale Phase 0 as --from; got Ok"
        );
        if let Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { version })) = result {
            assert_eq!(version, PHASE_ZERO_VERSION);
        } else {
            panic!("Expected StalePhaseZero refusal, got: {:?}", result);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn squash_refuses_legacy_generated_stale_phase_zero_as_from() {
        let root = temp_root("squash_legacy_stale_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_legacy_generated_stale_phase_zero(&dir);

        fs::write(
            dir.join("V20260101000001__init.sdjql"),
            "CREATE TABLE bar();",
        )
        .unwrap();

        let from_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&from_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_err(),
            "Squash must refuse legacy generated-stale Phase 0 as --from; got Ok"
        );
        if let Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { version })) = result {
            assert_eq!(version, PHASE_ZERO_VERSION);
        } else {
            panic!("Expected StalePhaseZero refusal, got: {:?}", result);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn squash_refuses_seed_capable_phase_zero_as_from() {
        let root = temp_root("squash_seed_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_seed_capable_phase_zero(&dir);

        fs::write(
            dir.join("V20260101000001__init.sdjql"),
            "CREATE TABLE bar();",
        )
        .unwrap();

        let from_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&from_path, PHASE_ZERO_VERSION);
        assert!(
            matches!(
                result,
                Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { .. }))
            ),
            "Squash must refuse seed-capable Phase 0 as --from; got {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn squash_refuses_markerless_seed_phase_zero_as_from() {
        let root = temp_root("squash_markerless_seed_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_markerless_seed_phase_zero(&dir);

        fs::write(
            dir.join("V20260101000001__init.sdjql"),
            "CREATE TABLE bar();",
        )
        .unwrap();

        let from_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&from_path, PHASE_ZERO_VERSION);
        assert!(
            matches!(
                result,
                Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { .. }))
            ),
            "Squash must refuse markerless seed Phase 0 as --from; got {result:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn squash_refuses_extended_seed_phase_zero_as_from() {
        for (name, statement) in extended_seed_statement_cases() {
            let root = temp_root(&format!("squash_extended_seed_p0_{name}"));
            let dir = root.join("migrations/main/billing");
            fs::create_dir_all(&dir).unwrap();
            write_phase_zero_with_seed_statement(&dir, statement);

            fs::write(
                dir.join("V20260101000001__init.sdjql"),
                "CREATE TABLE bar();",
            )
            .unwrap();

            let from_path = dir.join(up_filename(PHASE_ZERO_VERSION));
            let result = check_phase_zero_attune_entry(&from_path, PHASE_ZERO_VERSION);
            assert!(
                matches!(
                    result,
                    Err(AttuneError::Refused(AttuneRefusal::StalePhaseZero { .. }))
                ),
                "Squash must refuse extended seed Phase 0 {name} as --from; got {result:?}"
            );

            let _ = fs::remove_dir_all(&root);
        }
    }

    /// Identity-free production Phase 0 attune Squash passes.
    #[test]
    fn squash_accepts_identity_free_production_phase_zero_as_from() {
        let root = temp_root("squash_current_p0");
        let dir = root.join("migrations/main/billing");
        fs::create_dir_all(&dir).unwrap();
        write_current_production_phase_zero(&dir);

        // Add a later migration so squash would have multiple files.
        fs::write(
            dir.join("V20260101000001__init.sdjql"),
            "CREATE TABLE bar();",
        )
        .unwrap();

        let from_path = dir.join(up_filename(PHASE_ZERO_VERSION));
        let result = check_phase_zero_attune_entry(&from_path, PHASE_ZERO_VERSION);
        assert!(
            result.is_ok(),
            "Squash must accept identity-free Phase 0 as --from; got Err: {:?}",
            result
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// StalePhaseZero refusal message is actionable and names the version.
    #[test]
    fn u3_stale_phase_zero_refusal_message_names_version() {
        let r = AttuneRefusal::StalePhaseZero {
            version: PHASE_ZERO_VERSION.to_string(),
        };
        let s = format!("{r}");
        assert!(s.contains(PHASE_ZERO_VERSION), "must name the version: {s}");
        assert!(
            s.contains("seed-DML") && s.contains("generated-stale"),
            "must describe the stale-artifact contract: {s}"
        );
        assert!(
            s.contains("regenerate") || s.contains("composer"),
            "must suggest remediation: {s}"
        );
    }
}
