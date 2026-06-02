//! Three-way match logic for `build.rs` drift diagnostics.
//! Pure-data layer that takes the three
//! schema inputs to the build.rs three-way match and produces
//! the warnings to surface to the operator. No I/O, no SQL, no
//! filesystem — purely computing the diagnostic strings from typed
//! inputs.
//! # The three sources
//! Per the v3 plan §6 amendment:
//! 1. **`models`** — the descriptor inventory at compile time.
//!    Source: `target/djogi_models.json` written by `#[derive(Model)]`
//!    on every `cargo build`. In the build.rs caller this is parsed
//!    fresh every build; here we accept it as an `Option<&AppliedSchema>`
//!    keyed per-bucket.
//! 2. **`pending`** — the most recent composed (but not yet applied)
//!    delta. Source: `target/djogi_pending/<database>/<app>.json`
//!    written by `migrations compose`. `None` when no compose is
//!    pending.
//! 3. **`snapshot`** — the last successfully-applied schema. Source:
//!    `migrations/<database>/<app>/schema_snapshot.json` committed
//!    to the migrations submodule. `None` for a fresh
//!    `(database, app)` pair (zero migrations applied).
//! # The four outcomes
//! Per the v3 amendment, the match produces exactly one of these
//! outcomes for each bucket:
//! - **Outcome 1 — synced.** `models == pending == snapshot` (or
//!   pending is `None` and `models == snapshot`). No drift, no
//!   warning.
//! - **Outcome 2 — composed not applied.** `models == pending`,
//!   `snapshot != models`. Warning: a composed migration is pending
//!   apply.
//! - **Outcome 3 — drift.** `models != pending` AND `models !=
//! snapshot`. Warning: model drift; suggest `compose`.
//! - **Outcome 4 — pending invalid.** `pending != models` AND
//!   `pending != snapshot` AND `models != snapshot`. The pending
//!   file is stale — warning suggests re-running compose.
//!   The warning *wording* is frozen — see the `format_warning_*`
//!   helpers below — so the expectation-style integration test can
//!   match on exact stderr output.
//! # No regex
//! The match is structural — `==` between owned [`AppliedSchema`]
//! values. No string scanning, no regex.

use std::collections::BTreeMap;

use super::naming::MIGRATION_FILE_EXT;
use super::projection::BucketKey;
use super::schema::AppliedSchema;
use super::target::FilesystemBucket;

/// One drift diagnostic surfaced by the three-way match.
/// Carries both the bucket identity and the formatted warning text
/// so the build.rs caller can issue `cargo:warning=…` lines verbatim.
/// Build.rs callers concatenate `bucket` + `text` for log lines that
/// pinpoint the offending bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftDiagnostic {
    /// Which bucket the diagnostic applies to. Empty `app` denotes the
    /// synthetic global bucket; consumers print the bucket as
    /// `<database>/<dirname>` using
    /// [`super::target::app_dirname`].
    pub bucket: BucketKey,
    /// One of [`DriftKind`] — disambiguates the four outcomes plus
    /// the D004 folder-drift case.
    pub kind: DriftKind,
    /// Frozen, human-readable text — exactly what build.rs emits via
    /// `cargo:warning=…`. The text is pre-formatted at construction
    /// time so callers do not need to re-derive it.
    pub text: String,
}

/// Discriminant for a [`DriftDiagnostic`].
/// `Outcome2`, `Outcome3`, `Outcome4` correspond to the
/// "composed not applied", "drift", "pending invalid" outcomes.
/// `D004FilesystemUnregistered` and `D004RegisteredMissingFolder`
/// implement the v3 amendment D004 contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftKind {
    /// Outcome 2 — composed migration not yet applied. Models match
    /// the pending JSON, but the snapshot diverges. Operator runs
    /// `djogi migrations apply` to land the change.
    Outcome2ComposedNotApplied,
    /// Outcome 3 — model drift. Models diverge from BOTH pending and
    /// snapshot. Operator runs `djogi migrations compose` to stage
    /// the delta.
    Outcome3Drift,
    /// Outcome 4 — pending JSON is stale. Pending diverges from
    /// models AND from snapshot AND models also diverges from
    /// snapshot. Operator re-runs `djogi migrations compose` to
    /// refresh the pending file.
    Outcome4PendingInvalid,
    /// D004 — a `migrations/<database>/<app>/` directory exists on
    /// disk but the snapshot's `registered_apps` list does not
    /// include `<app>`. Either the declaring crate was removed
    /// without a tombstone migration or the snapshot is stale.
    D004FilesystemUnregistered,
    /// D004 — the snapshot's `registered_apps` list includes an app
    /// that has no corresponding `migrations/<database>/<app>/`
    /// directory on disk. Either the directory was hand-deleted or
    /// the migrations submodule was checked out at the wrong revision.
    D004RegisteredMissingFolder,
}

impl DriftKind {
    /// Only Outcome 3 (model drift) is muted by
    /// `Djogi.toml::build.suppress_drift_warning = true`. The other
    /// outcomes (D004 filesystem mismatches, Outcome 2
    /// composed-not-applied, Outcome 4 stale-pending) are
    /// operator-actionable signals that always print regardless of
    /// the suppression flag.
    /// The build.rs script uses an `is_outcome3_drift: bool` field
    /// on its internal diagnostic struct; this method gives library
    /// callers the same predicate without re-implementing the kind
    /// discriminant.
    pub fn is_outcome3_drift(self) -> bool {
        matches!(self, DriftKind::Outcome3Drift)
    }
}

/// Compute the three-way match outcome for one bucket.
/// `models`, `pending`, `snapshot` are the three inputs described in
/// the module-level docs. Returns `None` for Outcome 1 (synced)
/// callers treat `None` as "nothing to warn about".
/// Convenience wrapper around [`classify_bucket_with_pending`] that
/// supplies `None` for the pending-version argument. Outcome 2
/// messages from this entry point use the `<unknown>` placeholder.
/// The two entry points share a single implementation: this
/// function forwards directly to [`classify_bucket_with_pending`] so
/// the four outcome categories and frozen-string contracts cannot
/// drift between them. The `<unknown>` placeholder is the defensive
/// fallback for callers (e.g. early-build paths, malformed pending
/// JSON) that genuinely cannot supply a version; production paths
/// thread the real version through [`classify_bucket_with_pending`].
pub fn classify_bucket(
    bucket: &BucketKey,
    models: Option<&AppliedSchema>,
    pending: Option<&AppliedSchema>,
    snapshot: Option<&AppliedSchema>,
) -> Option<DriftDiagnostic> {
    classify_bucket_with_pending(bucket, models, pending, snapshot, None)
}

/// Same as [`classify_bucket`] but accepts the pending migration's
/// version ID (e.g. `"V20260425010203__add_widgets"`) so the Outcome
/// 2 warning includes the filename + version (per).
/// **Caller mapping:**
/// - `migrations status` (CLI): threads the real `version` from each
///   pending JSON it loads; the operator sees an actionable filename.
/// - `build.rs`: walks pending JSON files itself (build scripts cannot
///   import the crate they are compiling), reads the `version` field
///   directly, and emits the same wording. The
///   `phase7_t6_build_warning_agreement` integration test pins the
///   agreement byte-for-byte.
/// - `build_classify_bucket` re-export (the convenience wrapper): used
///   by tests and the rare caller that genuinely lacks a version
///   string. Surfaces the `<unknown>` placeholder so the message is
///   still well-formed.
///   Both entry points route through this function so the four-outcome
///   classification logic lives in one place.
pub fn classify_bucket_with_pending(
    bucket: &BucketKey,
    models: Option<&AppliedSchema>,
    pending: Option<&AppliedSchema>,
    snapshot: Option<&AppliedSchema>,
    pending_version: Option<&str>,
) -> Option<DriftDiagnostic> {
    // The three-way comparison ignores `generated_at` because every
    // run regenerates the timestamp; comparing it would always trip
    // drift on a fresh build. The comparator here masks the field on
    // both sides.
    let models_eq_pending = match (models, pending) {
        (Some(m), Some(p)) => schema_equiv(m, p),
        (None, None) => true,
        _ => false,
    };
    let models_eq_snapshot = match (models, snapshot) {
        (Some(m), Some(s)) => schema_equiv(m, s),
        (None, None) => true,
        _ => false,
    };
    let pending_eq_snapshot = match (pending, snapshot) {
        (Some(p), Some(s)) => schema_equiv(p, s),
        (None, None) => true,
        _ => false,
    };

    // Outcome 1 — fully synced. Two valid shapes:
    // (a) no pending, models == snapshot.
    // (b) pending present, models == pending == snapshot.
    if pending.is_none() && models_eq_snapshot {
        return None;
    }
    if pending.is_some() && models_eq_pending && models_eq_snapshot {
        return None;
    }

    // Outcome 2 — composed not yet applied. models == pending,
    // snapshot != models.
    if pending.is_some() && models_eq_pending && !models_eq_snapshot {
        return Some(DriftDiagnostic {
            bucket: bucket.clone(),
            kind: DriftKind::Outcome2ComposedNotApplied,
            text: format_warning_outcome2(bucket, pending_version),
        });
    }

    // Outcome 4 — pending invalid. pending diverges from BOTH models
    // and snapshot. (Per amendment: "pending diverges from
    // models, but snapshot != pending too".) Models also diverges
    // from snapshot — otherwise the Outcome-3 case would fire instead.
    if pending.is_some() && !models_eq_pending && !pending_eq_snapshot {
        return Some(DriftDiagnostic {
            bucket: bucket.clone(),
            kind: DriftKind::Outcome4PendingInvalid,
            text: format_warning_outcome4(bucket),
        });
    }

    // Outcome 3 — model drift. Models diverge from BOTH pending and
    // snapshot. Reaching this branch means: pending is None (the
    // typical fresh-drift case) OR pending equals snapshot (compose
    // already ran but models has since drifted further).
    if !models_eq_snapshot && (pending.is_none() || pending_eq_snapshot) {
        return Some(DriftDiagnostic {
            bucket: bucket.clone(),
            kind: DriftKind::Outcome3Drift,
            text: format_warning_outcome3(bucket),
        });
    }

    // Fall-through — only reached when none of the above patterns
    // match. Treat as a generic drift warning so the operator at
    // least gets a hint.
    Some(DriftDiagnostic {
        bucket: bucket.clone(),
        kind: DriftKind::Outcome3Drift,
        text: format_warning_outcome3(bucket),
    })
}

/// Compute D004 (folder drift) diagnostics for a database scan.
/// `filesystem` is the result of [`super::target::scan_filesystem`].
/// `snapshots` carries the per-bucket `registered_apps` for every
/// `(database, app)` pair the snapshot tree knows about. Walks both
/// directions:
/// 1. Each filesystem bucket whose `(database, app)` is not in
///    `snapshots` (under any registered_apps list for that database)
///    surfaces as `D004FilesystemUnregistered`.
/// 2. Each `registered_apps` entry that has no matching filesystem
///    directory surfaces as `D004RegisteredMissingFolder`.
///    The snapshot's `registered_apps` list is the source of truth for
///    "what apps existed when the snapshot was last saved". Walking it
///    per-database ensures the diagnostic surfaces the right database
///    even when one DB has fully-synced apps and another is drifting.
pub fn classify_filesystem_drift(
    filesystem: &std::collections::BTreeSet<FilesystemBucket>,
    snapshots: &BTreeMap<BucketKey, AppliedSchema>,
) -> Vec<DriftDiagnostic> {
    let mut diagnostics = Vec::new();

    // Build per-database registered_apps lookup from snapshots. All
    // snapshots within the same database share the same
    // registered_apps list (the projection emits it consistently),
    // but we union across all snapshots to be safe.
    let mut registered_per_db: BTreeMap<String, std::collections::BTreeSet<String>> =
        BTreeMap::new();
    for (key, snap) in snapshots {
        let entry = registered_per_db.entry(key.database.clone()).or_default();
        for app in &snap.registered_apps {
            entry.insert(app.clone());
        }
    }

    // Direction 1 — filesystem dir without a registered_apps entry.
    for fs_bucket in filesystem {
        let known_apps = registered_per_db.get(&fs_bucket.database);
        let is_registered = known_apps
            .map(|set| set.contains(&fs_bucket.app))
            .unwrap_or(false);
        if !is_registered {
            let bucket = BucketKey {
                database: fs_bucket.database.clone(),
                app: fs_bucket.app.clone(),
            };
            diagnostics.push(DriftDiagnostic {
                kind: DriftKind::D004FilesystemUnregistered,
                text: format_warning_d004_unregistered(&bucket),
                bucket,
            });
        }
    }

    // Direction 2 — registered_apps entry without a filesystem dir.
    let fs_lookup: std::collections::BTreeSet<(String, String)> = filesystem
        .iter()
        .map(|b| (b.database.clone(), b.app.clone()))
        .collect();
    for (database, apps) in &registered_per_db {
        for app in apps {
            if !fs_lookup.contains(&(database.clone(), app.clone())) {
                let bucket = BucketKey {
                    database: database.clone(),
                    app: app.clone(),
                };
                diagnostics.push(DriftDiagnostic {
                    kind: DriftKind::D004RegisteredMissingFolder,
                    text: format_warning_d004_missing(&bucket),
                    bucket,
                });
            }
        }
    }

    diagnostics
}

// ── Frozen warning wording ─────────────────────────────────────────────────
// The `phase7_t6_build_warning_agreement` integration test pins these
// exact strings. Any rewording here breaks the stored expectation;
// bump the version IDs in that test in lockstep when intentionally
// changing the messages.
// MIRROR: keep in lockstep with djogi/build.rs (the build-script re-implements these formatters)

/// Outcome 2 — `composed migration not yet applied: <filename>
/// (version <version>; bucket <database>/<app>)` (frozen wording,
/// per amendment).
/// `pending_version` carries the pending migration's version ID
/// (e.g. `"V20260425010203__add_widgets"`); the `.sdjql` filename is
/// derived by appending the up-side suffix. Callers that don't have
/// the version pass `None` and the message uses the placeholder
/// `<unknown>` for filename and version — production code paths
/// always supply a real version.
pub fn format_warning_outcome2(bucket: &BucketKey, pending_version: Option<&str>) -> String {
    let (filename, version) = match pending_version {
        Some(v) => (format!("{v}{ext}", ext = MIGRATION_FILE_EXT), v.to_string()),
        None => (
            "<unknown>".to_string() + MIGRATION_FILE_EXT,
            "<unknown>".to_string(),
        ),
    };
    format!(
        "composed migration not yet applied: {filename} (version {version}; bucket {database}/{app})",
        database = bucket.database,
        app = super::target::app_dirname(&bucket.app),
    )
}

/// Outcome 3 — drift detected (frozen).
pub fn format_warning_outcome3(bucket: &BucketKey) -> String {
    format!(
        "model drift detected for {database}/{app}; run `djogi migrations compose` to stage the delta",
        database = bucket.database,
        app = super::target::app_dirname(&bucket.app),
    )
}

/// Outcome 4 — pending JSON is stale (frozen).
pub fn format_warning_outcome4(bucket: &BucketKey) -> String {
    format!(
        "pending compose for {database}/{app} is stale relative to model state; re-run `djogi migrations compose`",
        database = bucket.database,
        app = super::target::app_dirname(&bucket.app),
    )
}

/// D004 — filesystem app not registered in snapshot (frozen).
pub fn format_warning_d004_unregistered(bucket: &BucketKey) -> String {
    format!(
        "D004: filesystem app \"{database}/{app}\" not registered in snapshot",
        database = bucket.database,
        app = super::target::app_dirname(&bucket.app),
    )
}

/// D004 — registered app missing from filesystem (frozen).
pub fn format_warning_d004_missing(bucket: &BucketKey) -> String {
    format!(
        "D004: registered app \"{database}/{app}\" missing from filesystem",
        database = bucket.database,
        app = super::target::app_dirname(&bucket.app),
    )
}

/// Equality between two snapshots, modulo the `generated_at` field.
/// `generated_at` carries the wall-clock time at projection; comparing
/// it across runs would always show drift on a clean build. We
/// compare every other field structurally.
fn schema_equiv(a: &AppliedSchema, b: &AppliedSchema) -> bool {
    a.djogi_version == b.djogi_version
        && a.enums == b.enums
        && a.format_version == b.format_version
        && a.indexes == b.indexes
        && a.models == b.models
        && a.registered_apps == b.registered_apps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::schema::SNAPSHOT_FORMAT_VERSION;
    use std::collections::BTreeMap;

    fn empty_schema() -> AppliedSchema {
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        }
    }

    fn drifted_schema() -> AppliedSchema {
        // Different djogi_version is enough to make !=. The schema
        // shape is otherwise the same.
        AppliedSchema {
            djogi_version: "9.9.9".to_string(),
            ..empty_schema()
        }
    }

    fn global_bucket() -> BucketKey {
        BucketKey {
            database: "main".into(),
            app: "".into(),
        }
    }

    #[test]
    fn outcome1_no_pending_models_match_snapshot_returns_none() {
        let m = empty_schema();
        let s = empty_schema();
        let diag = classify_bucket(&global_bucket(), Some(&m), None, Some(&s));
        assert!(diag.is_none());
    }

    #[test]
    fn outcome1_all_three_match_returns_none() {
        let m = empty_schema();
        let p = empty_schema();
        let s = empty_schema();
        let diag = classify_bucket(&global_bucket(), Some(&m), Some(&p), Some(&s));
        assert!(diag.is_none());
    }

    #[test]
    fn outcome2_composed_not_applied() {
        let m = drifted_schema();
        let p = drifted_schema();
        let s = empty_schema();
        // Default classify_bucket entry point — no pending version
        // so the message uses the `<unknown>` placeholder.
        let diag = classify_bucket(&global_bucket(), Some(&m), Some(&p), Some(&s)).expect("diag");
        assert_eq!(diag.kind, DriftKind::Outcome2ComposedNotApplied);
        assert_eq!(
            diag.text,
            "composed migration not yet applied: <unknown>.sdjql (version <unknown>; bucket main/_global_)"
        );
    }

    #[test]
    fn outcome2_composed_not_applied_with_pending_version() {
        // ,
        // the message names both the migration filename AND the version ID
        // alongside the bucket. Production build.rs / status callers
        // always supply this.
        let m = drifted_schema();
        let p = drifted_schema();
        let s = empty_schema();
        let diag = classify_bucket_with_pending(
            &global_bucket(),
            Some(&m),
            Some(&p),
            Some(&s),
            Some("V20260425010203__add_widgets"),
        )
        .expect("diag");
        assert_eq!(diag.kind, DriftKind::Outcome2ComposedNotApplied);
        assert_eq!(
            diag.text,
            "composed migration not yet applied: V20260425010203__add_widgets.sdjql \
              (version V20260425010203__add_widgets; bucket main/_global_)"
        );
    }

    #[test]
    fn outcome3_drift_no_pending() {
        let m = drifted_schema();
        let s = empty_schema();
        let diag = classify_bucket(&global_bucket(), Some(&m), None, Some(&s)).expect("diag");
        assert_eq!(diag.kind, DriftKind::Outcome3Drift);
        assert_eq!(
            diag.text,
            "model drift detected for main/_global_; run `djogi migrations compose` to stage the delta"
        );
    }

    #[test]
    fn outcome4_pending_invalid() {
        // models drifted from snapshot; pending also drifted but
        // disagreed with models.
        let m = drifted_schema();
        let p = AppliedSchema {
            djogi_version: "5.5.5".to_string(),
            ..empty_schema()
        };
        let s = empty_schema();
        let diag = classify_bucket(&global_bucket(), Some(&m), Some(&p), Some(&s)).expect("diag");
        assert_eq!(diag.kind, DriftKind::Outcome4PendingInvalid);
        assert_eq!(
            diag.text,
            "pending compose for main/_global_ is stale relative to model state; re-run `djogi migrations compose`"
        );
    }

    #[test]
    fn d004_filesystem_unregistered() {
        let mut snapshots = BTreeMap::new();
        let mut snap = empty_schema();
        snap.registered_apps = vec!["".to_string(), "billing".to_string()];
        snapshots.insert(global_bucket(), snap);

        let fs: std::collections::BTreeSet<FilesystemBucket> = [
            FilesystemBucket {
                database: "main".into(),
                app: "".into(),
            },
            FilesystemBucket {
                database: "main".into(),
                app: "billing".into(),
            },
            FilesystemBucket {
                database: "main".into(),
                app: "ghost".into(),
            },
        ]
        .into_iter()
        .collect();
        let diags = classify_filesystem_drift(&fs, &snapshots);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind, DriftKind::D004FilesystemUnregistered);
        assert_eq!(
            diags[0].text,
            "D004: filesystem app \"main/ghost\" not registered in snapshot"
        );
    }

    #[test]
    fn d004_registered_missing_folder() {
        let mut snapshots = BTreeMap::new();
        let mut snap = empty_schema();
        snap.registered_apps = vec!["".to_string(), "billing".to_string()];
        snapshots.insert(global_bucket(), snap);

        let fs: std::collections::BTreeSet<FilesystemBucket> = [FilesystemBucket {
            database: "main".into(),
            app: "".into(),
        }]
        .into_iter()
        .collect();
        let diags = classify_filesystem_drift(&fs, &snapshots);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind, DriftKind::D004RegisteredMissingFolder);
        assert_eq!(
            diags[0].text,
            "D004: registered app \"main/billing\" missing from filesystem"
        );
    }

    #[test]
    fn schema_equiv_ignores_generated_at() {
        let mut a = empty_schema();
        a.generated_at = "2020-01-01T00:00:00Z".into();
        let b = empty_schema();
        assert!(schema_equiv(&a, &b));
    }

    #[test]
    fn fresh_snapshot_no_pending_no_models_synced() {
        // The fresh-project case — no models registered yet, nothing
        // pending, nothing in the snapshot. Should be Outcome 1.
        let diag = classify_bucket(&global_bucket(), None, None, None);
        assert!(diag.is_none());
    }
}
