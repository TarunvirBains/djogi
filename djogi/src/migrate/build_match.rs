//! Three-way match logic for `build.rs` drift diagnostics.
//! Pure-data layer that takes the three
//! schema inputs to the build.rs three-way match and produces
//! the warnings to surface to the operator. No I/O, no SQL, no
//! filesystem — purely computing the diagnostic strings from typed
//! inputs.
//! # The three sources
//! Per the v3 plan §6 amendment:
//! 1. **`models`** — the descriptor inventory at compile time.
//! Source: `target/djogi_models.json` written by `#[derive(Model)]`
//! on every `cargo build`. In the build.rs caller this is parsed
//! fresh every build; here we accept it as an `Option<&AppliedSchema>`
//! keyed per-bucket.
//! 2. **`pending`** — a composed (but not yet applied) delta selected
//! for the public bucket. Source:
//! `target/djogi_pending/<database>/<app>.json` plus the hidden
//! Phase 0 namespace
//! `target/djogi_pending/<database>/.phase_zero/<version>.json`
//! written by `migrations compose`. Build-side ingestion keeps
//! those two source kinds distinct before selecting one pending
//! artifact for the `(database, app)` outcome. `None` when no
//! compose is pending.
//! 3. **`snapshot`** — the last successfully-applied schema. Source:
//! `migrations/<database>/<app>/schema_snapshot.json` committed
//! to the migrations submodule. `None` for a fresh
//! `(database, app)` pair (zero migrations applied).
//! # The four outcomes
//! Per the v3 amendment, the match produces exactly one of these
//! outcomes for each bucket:
//! - **Outcome 1 — synced.** `models == pending == snapshot` (or
//! pending is `None` and `models == snapshot`). No drift, no
//! warning.
//! - **Outcome 2 — composed not applied.** `models == pending`,
//! `snapshot != models`. Warning: a composed migration is pending
//! apply.
//! - **Outcome 3 — drift.** `models != pending` AND `models !=
//! snapshot`. Warning: model drift; suggest `compose`.
//! - **Outcome 4 — pending invalid.** `pending != models` AND
//! `pending != snapshot` AND `models != snapshot`. The pending
//! file is stale — warning suggests re-running compose.
//! The warning *wording* is frozen — see the `format_warning_*`
//! helpers below — so the expectation-style integration test can
//! match on exact stderr output.
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

/// Source kind for a pending artifact loaded before build-match
/// classification. Hidden Phase 0 and normal global pending can share
/// the same public `(database, app)` bucket while remaining distinct
/// ingestion identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingArtifactKind {
    /// `target/djogi_pending/<database>/<app>.json`.
    Normal,
    /// `target/djogi_pending/<database>/.phase_zero/<version>.json`.
    HiddenPhaseZero,
}

/// Pending artifact input for callers that need to preserve the
/// source kind through selection before classifying one public bucket.
#[derive(Debug, Clone, Copy)]
pub struct PendingArtifact<'a> {
    /// Where this pending artifact came from.
    pub kind: PendingArtifactKind,
    /// Snapshot embedded in the pending JSON.
    pub schema: &'a AppliedSchema,
    /// Pending migration version, used for Outcome 2 wording.
    pub version: Option<&'a str>,
}

/// State of the descriptor inventory (`target/djogi_models.json`) for
/// a single classification pass.
///
/// # Why this exists
///
/// The three-way match in [`classify_bucket_with_pending_artifact`]
/// compares `models` against the pending plan and the committed
/// snapshot. Those comparisons are only meaningful when the inventory
/// is actually available. Representing "the inventory file is missing"
/// with a per-bucket `models: None` conflates two genuinely different
/// situations:
///
/// - **Inventory present, this bucket declares no models** — a real,
/// model-relative `None` the classifier can reason about.
/// - **Inventory unavailable for the whole build** — there is no basis
/// for *any* model-relative assertion, so emitting Outcome 3 / 4
/// ("drift" / "stale pending") would misdirect the operator.
///
/// `ModelInventory` makes that distinction a first-class input so the
/// classifier never launders an unavailable inventory into a confident
/// model-relative verdict.
///
/// # When to reach for it
///
/// Use [`classify_bucket_with_inventory`] with this enum when the
/// caller knows whether the inventory file was readable as a whole
/// (e.g. `build.rs`, which reads `target/djogi_models.json` once per
/// build). Callers that always have a present inventory (e.g.
/// `migrations status`, which projects the live `inventory` registry)
/// can keep using [`classify_bucket`] and friends, which thread
/// [`ModelInventory::Present`] for them.
///
/// # Example
///
/// ```
/// use djogi::migrate::build_match::{
///  ModelInventory, PendingArtifact, PendingArtifactKind,
///  classify_bucket_with_inventory,
/// };
/// use djogi::migrate::projection::BucketKey;
/// use djogi::migrate::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
/// use std::collections::BTreeMap;
///
/// fn schema(tag: &str) -> AppliedSchema {
///  AppliedSchema {
///   djogi_version: tag.to_string(),
///   enums: BTreeMap::new(),
///   format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
///   generated_at: "2026-04-25T00:00:00Z".to_string(),
///   indexes: Vec::new(),
///   models: BTreeMap::new(),
///   registered_apps: vec![String::new()],
///  }
/// }
///
/// let bucket = BucketKey { database: "main".into(), app: String::new() };
/// let pending = schema("pending");
/// let snapshot = schema("snapshot");
/// let artifact = PendingArtifact {
///  kind: PendingArtifactKind::HiddenPhaseZero,
///  schema: &pending,
///  version: Some("V00000000000000__phase_zero_bootstrap"),
/// };
///
/// // Inventory unavailable: a composed-but-unapplied pending must read
/// // as "apply me", never as "stale / drift".
/// let diag = classify_bucket_with_inventory(
///  &bucket,
///  ModelInventory::Absent,
///  Some(artifact),
///  Some(&snapshot),
/// )
///.expect("composed-not-applied diagnostic");
/// assert!(diag.text.contains("composed migration not yet applied"));
/// ```
#[derive(Debug, Clone, Copy)]
pub enum ModelInventory<'a> {
    /// The inventory file is unavailable for this build — either
    /// legitimately missing (the typical fresh-adoption state) or
    /// malformed (unreadable / not a valid JSON object / carrying a
    /// malformed bucket key). Either way there is no trustworthy model
    /// state, so only the pending↔snapshot relationship is classified.
    ///
    /// The *missing* vs *malformed* distinction is a read-site I/O
    /// concern (whether to emit a loud warning), not a per-bucket
    /// classification concern, so it is kept out of this enum and owned
    /// by the caller that performed the read.
    Absent,
    /// The inventory file is present and valid. The wrapped
    /// `Option<&AppliedSchema>` is this bucket's model state — `Some`
    /// when the bucket declares models, `None` when a present inventory
    /// simply has no entry for the bucket.
    Present(Option<&'a AppliedSchema>),
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
/// pending JSON it loads; the operator sees an actionable filename.
/// - `build.rs`: walks pending JSON files itself (build scripts cannot
/// import the crate they are compiling), reads the `version` field
/// directly, and emits the same wording. The
/// `phase7_t6_build_warning_agreement` integration test pins the
/// agreement byte-for-byte.
/// - `build_classify_bucket` re-export (the convenience wrapper): used
/// by tests and the rare caller that genuinely lacks a version
/// string. Surfaces the `<unknown>` placeholder so the message is
/// still well-formed.
/// Both entry points route through this function so the four-outcome
/// classification logic lives in one place.
pub fn classify_bucket_with_pending(
    bucket: &BucketKey,
    models: Option<&AppliedSchema>,
    pending: Option<&AppliedSchema>,
    snapshot: Option<&AppliedSchema>,
    pending_version: Option<&str>,
) -> Option<DriftDiagnostic> {
    let pending = pending.map(|schema| PendingArtifact {
        kind: PendingArtifactKind::Normal,
        schema,
        version: pending_version,
    });
    classify_bucket_with_pending_artifact(bucket, models, pending, snapshot)
}

/// Inventory-aware classification entry point.
///
/// This is the canonical classifier: it threads an explicit
/// [`ModelInventory`] so an *unavailable* inventory (file missing or
/// malformed) cannot be silently treated as a confident model-relative
/// verdict. [`classify_bucket`], [`classify_bucket_with_pending`], and
/// [`classify_bucket_with_pending_artifact`] are
/// [`ModelInventory::Present`] wrappers around this function, so their
/// behaviour — and every pinned warning string — is unchanged.
///
/// # Behaviour
///
/// - [`ModelInventory::Present`] runs the full three-way match
/// (Outcomes 1–4) exactly as [`classify_bucket_with_pending_artifact`]
/// always has.
/// - [`ModelInventory::Absent`] runs the **reduced** classification:
/// the model-vs-* legs are skipped (there is no trustworthy model
/// state), and only the pending↔snapshot relationship is reported:
/// - pending present and `pending != snapshot` →
///  [`DriftKind::Outcome2ComposedNotApplied`] (apply the composed
///  migration);
/// - pending present and `pending == snapshot` → `None` (synced);
/// - no pending → `None` (the snapshot↔filesystem D004 leg owns any
///  remaining signal).
///
/// Returns `None` whenever there is nothing to warn about.
///
/// # Example
///
/// See [`ModelInventory`] for a runnable example.
pub fn classify_bucket_with_inventory(
    bucket: &BucketKey,
    inventory: ModelInventory<'_>,
    pending: Option<PendingArtifact<'_>>,
    snapshot: Option<&AppliedSchema>,
) -> Option<DriftDiagnostic> {
    match inventory {
        ModelInventory::Present(models) => {
            classify_bucket_with_pending_artifact(bucket, models, pending, snapshot)
        }
        ModelInventory::Absent => classify_inventory_absent(bucket, pending, snapshot),
    }
}

/// Reduced classification used when the model inventory is unavailable
/// ([`ModelInventory::Absent`]).
///
/// Without a trustworthy model state the model-vs-pending and
/// model-vs-snapshot legs are meaningless, so this only inspects the
/// pending↔snapshot relationship. A composed-but-unapplied pending must
/// read as "apply me" (Outcome 2), never as drift / stale-pending,
/// which would misdirect a fresh compose/apply workflow.
fn classify_inventory_absent(
    bucket: &BucketKey,
    pending: Option<PendingArtifact<'_>>,
    snapshot: Option<&AppliedSchema>,
) -> Option<DriftDiagnostic> {
    let Some(artifact) = pending else {
        // No composed delta pending — the snapshot↔filesystem (D004)
        // leg owns any remaining signal; nothing model-relative here.
        return None;
    };
    let pending_eq_snapshot = match snapshot {
        Some(s) => schema_equiv(artifact.schema, s),
        None => false,
    };
    if pending_eq_snapshot {
        // Composed delta already equals the applied snapshot — synced.
        return None;
    }
    Some(DriftDiagnostic {
        bucket: bucket.clone(),
        kind: DriftKind::Outcome2ComposedNotApplied,
        text: format_warning_outcome2(bucket, artifact.version),
    })
}

/// Same classifier as [`classify_bucket_with_pending`], but carries a
/// kind-aware pending artifact chosen by the caller. This keeps the
/// build-side hidden Phase 0 identity internal while preserving one
/// diagnostic outcome per public `(database, app)` bucket.
///
/// This is the [`ModelInventory::Present`] implementation;
/// [`classify_bucket_with_inventory`] is the inventory-aware entry
/// point that also handles [`ModelInventory::Absent`].
pub fn classify_bucket_with_pending_artifact(
    bucket: &BucketKey,
    models: Option<&AppliedSchema>,
    pending: Option<PendingArtifact<'_>>,
    snapshot: Option<&AppliedSchema>,
) -> Option<DriftDiagnostic> {
    let pending_schema = pending.map(|artifact| artifact.schema);
    let pending_version = pending.and_then(|artifact| artifact.version);

    // The three-way comparison ignores `generated_at` because every
    // run regenerates the timestamp; comparing it would always trip
    // drift on a fresh build. The comparator here masks the field on
    // both sides.
    let models_eq_pending = match (models, pending_schema) {
        (Some(m), Some(p)) => schema_equiv(m, p),
        (None, None) => true,
        _ => false,
    };
    let models_eq_snapshot = match (models, snapshot) {
        (Some(m), Some(s)) => schema_equiv(m, s),
        (None, None) => true,
        _ => false,
    };
    let pending_eq_snapshot = match (pending_schema, snapshot) {
        (Some(p), Some(s)) => schema_equiv(p, s),
        (None, None) => true,
        _ => false,
    };

    // Outcome 1 — fully synced. Two valid shapes:
    // (a) no pending, models == snapshot.
    // (b) pending present, models == pending == snapshot.
    if pending_schema.is_none() && models_eq_snapshot {
        return None;
    }
    if pending_schema.is_some() && models_eq_pending && models_eq_snapshot {
        return None;
    }

    // Outcome 2 — composed not yet applied. models == pending,
    // snapshot != models.
    if pending_schema.is_some() && models_eq_pending && !models_eq_snapshot {
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
    if pending_schema.is_some() && !models_eq_pending && !pending_eq_snapshot {
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
    if !models_eq_snapshot && (pending_schema.is_none() || pending_eq_snapshot) {
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
/// `snapshots` (under any registered_apps list for that database)
/// surfaces as `D004FilesystemUnregistered`.
/// 2. Each `registered_apps` entry that has no matching filesystem
/// directory surfaces as `D004RegisteredMissingFolder`.
/// The snapshot's `registered_apps` list is the source of truth for
/// "what apps existed when the snapshot was last saved". Walking it
/// per-database ensures the diagnostic surfaces the right database
/// even when one DB has fully-synced apps and another is drifting.
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

/// Malformed model-inventory warning (frozen).
///
/// # What
///
/// One loud, file-level warning emitted when `target/djogi_models.json`
/// exists but cannot be trusted: it is unreadable, is not valid JSON,
/// is not a JSON object, or carries a bucket key that is not in
/// `<database>/<app>` form. `path` names the offending file and
/// `detail` names the specific failure shape
/// (e.g. `"not a JSON object"`, `"invalid JSON: …"`,
/// `"unreadable: …"`, `` "bucket key \"x\" is not in <database>/<app> form" ``).
///
/// # Why
///
/// A *missing* inventory is the legitimate fresh-adoption state and is
/// silent. A *malformed* inventory is a broken file: silently treating
/// it as missing would launder an error into the legitimate skip path.
/// The build still completes — model state is treated as unavailable
/// and the model-vs-snapshot checks degrade to the same reduced
/// pending↔snapshot classification as [`ModelInventory::Absent`] — but
/// this warning makes the degradation explicit rather than silent.
///
/// # Where
///
/// Emitted once per build by `build.rs` as a `cargo:warning=` line.
/// This is the single source of truth for the wording; the build
/// script mirrors the same template byte-for-byte, pinned by the
/// `phase7_t6_build_warning_agreement` integration test.
///
/// # Example
///
/// ```
/// use djogi::migrate::build_match::format_warning_inventory_malformed;
///
/// let text =
///  format_warning_inventory_malformed("target/djogi_models.json", "not a JSON object");
/// assert!(text.contains("target/djogi_models.json"));
/// assert!(text.contains("not a JSON object"));
/// ```
pub fn format_warning_inventory_malformed(path: &str, detail: &str) -> String {
    format!(
        "descriptor inventory at {path} is malformed ({detail}); model state is treated as unavailable, so model-vs-snapshot checks are skipped for this build"
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
        //,
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
    fn outcome2_composed_not_applied_with_kind_aware_pending_artifact() {
        let m = drifted_schema();
        let p = drifted_schema();
        let s = empty_schema();
        let pending = PendingArtifact {
            kind: PendingArtifactKind::HiddenPhaseZero,
            schema: &p,
            version: Some("V00000000000000__phase_zero_bootstrap"),
        };

        let diag = classify_bucket_with_pending_artifact(
            &global_bucket(),
            Some(&m),
            Some(pending),
            Some(&s),
        )
        .expect("diag");

        assert_eq!(diag.kind, DriftKind::Outcome2ComposedNotApplied);
        assert_eq!(
            diag.text,
            "composed migration not yet applied: V00000000000000__phase_zero_bootstrap.sdjql \
    (version V00000000000000__phase_zero_bootstrap; bucket main/_global_)"
        );
    }

    #[test]
    fn classify_bucket_with_pending_remains_compatibility_wrapper() {
        let m = drifted_schema();
        let p = drifted_schema();
        let s = empty_schema();

        let wrapper = classify_bucket_with_pending(
            &global_bucket(),
            Some(&m),
            Some(&p),
            Some(&s),
            Some("V20260425010203__add_widgets"),
        )
        .expect("wrapper diag");
        let kind_aware = classify_bucket_with_pending_artifact(
            &global_bucket(),
            Some(&m),
            Some(PendingArtifact {
                kind: PendingArtifactKind::Normal,
                schema: &p,
                version: Some("V20260425010203__add_widgets"),
            }),
            Some(&s),
        )
        .expect("kind-aware diag");

        assert_eq!(wrapper, kind_aware);
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

    fn pending_artifact<'a>(
        schema: &'a AppliedSchema,
        version: &'static str,
    ) -> PendingArtifact<'a> {
        PendingArtifact {
            kind: PendingArtifactKind::HiddenPhaseZero,
            schema,
            version: Some(version),
        }
    }

    #[test]
    fn classify_inventory_absent_with_pending_is_composed_not_applied() {
        // Inventory unavailable + a composed pending that diverges from
        // the snapshot must read as Outcome 2 ("apply me"), NOT Outcome
        // 4 ("stale / re-run compose"). This is the fresh compose/apply
        // workflow the missing-inventory bug used to misdirect.
        let pending = drifted_schema();
        let snapshot = empty_schema();
        let diag = classify_bucket_with_inventory(
            &global_bucket(),
            ModelInventory::Absent,
            Some(pending_artifact(
                &pending,
                "V00000000000000__phase_zero_bootstrap",
            )),
            Some(&snapshot),
        )
        .expect("composed-not-applied diagnostic");
        assert_eq!(diag.kind, DriftKind::Outcome2ComposedNotApplied);
        assert_eq!(
            diag.text,
            "composed migration not yet applied: V00000000000000__phase_zero_bootstrap.sdjql \
    (version V00000000000000__phase_zero_bootstrap; bucket main/_global_)"
        );
    }

    #[test]
    fn classify_inventory_absent_pending_equals_snapshot_is_synced() {
        // Inventory unavailable but the composed pending already equals
        // the applied snapshot → nothing to apply → silent.
        let pending = empty_schema();
        let snapshot = empty_schema();
        let diag = classify_bucket_with_inventory(
            &global_bucket(),
            ModelInventory::Absent,
            Some(pending_artifact(
                &pending,
                "V00000000000000__phase_zero_bootstrap",
            )),
            Some(&snapshot),
        );
        assert!(
            diag.is_none(),
            "pending == snapshot must be synced: {diag:?}"
        );
    }

    #[test]
    fn classify_inventory_absent_no_pending_is_silent() {
        // Inventory unavailable and no pending compose: the D004
        // snapshot↔filesystem leg owns any remaining signal — nothing
        // model-relative to report here.
        let snapshot = empty_schema();
        let diag = classify_bucket_with_inventory(
            &global_bucket(),
            ModelInventory::Absent,
            None,
            Some(&snapshot),
        );
        assert!(
            diag.is_none(),
            "absent inventory + no pending must be silent: {diag:?}"
        );
    }

    #[test]
    fn classify_inventory_absent_no_pending_no_snapshot_is_silent() {
        // The truly-empty fresh project under an unavailable inventory.
        let diag =
            classify_bucket_with_inventory(&global_bucket(), ModelInventory::Absent, None, None);
        assert!(diag.is_none());
    }

    #[test]
    fn classify_inventory_present_empty_bucket_unchanged() {
        // INVERSE GUARD: a *present* inventory that simply has no entry
        // for this bucket (`Present(None)`) must keep its legacy
        // semantics — a diverging pending here is still Outcome 4. Only
        // the file-level Absent case moves off Outcome 4; per-bucket
        // `None` under a present inventory is untouched.
        let pending = drifted_schema();
        let snapshot = empty_schema();
        let diag = classify_bucket_with_inventory(
            &global_bucket(),
            ModelInventory::Present(None),
            Some(pending_artifact(
                &pending,
                "V00000000000000__phase_zero_bootstrap",
            )),
            Some(&snapshot),
        )
        .expect("present-empty bucket diagnostic");
        assert_eq!(
            diag.kind,
            DriftKind::Outcome4PendingInvalid,
            "Present(None) must preserve legacy Outcome 4, not be reclassified as absent: {diag:?}"
        );
    }

    #[test]
    fn classify_inventory_present_forwards_to_legacy_classifier() {
        // The Present arm must be byte-identical to the legacy
        // kind-aware classifier — the wrappers cannot drift.
        let m = drifted_schema();
        let p = drifted_schema();
        let s = empty_schema();
        let via_inventory = classify_bucket_with_inventory(
            &global_bucket(),
            ModelInventory::Present(Some(&m)),
            Some(pending_artifact(&p, "V20260425010203__add_widgets")),
            Some(&s),
        );
        let via_legacy = classify_bucket_with_pending_artifact(
            &global_bucket(),
            Some(&m),
            Some(pending_artifact(&p, "V20260425010203__add_widgets")),
            Some(&s),
        );
        assert_eq!(via_inventory, via_legacy);
    }

    #[test]
    fn inventory_malformed_warning_text_is_frozen() {
        // RED-A5 — the malformed-inventory warning is a frozen contract,
        // blessed once. The build.rs mirror is pinned against this exact
        // template by `phase7_t6_build_warning_agreement`.
        assert_eq!(
            format_warning_inventory_malformed("target/djogi_models.json", "not a JSON object"),
            "descriptor inventory at target/djogi_models.json is malformed (not a JSON object); \
    model state is treated as unavailable, so model-vs-snapshot checks are skipped for this build"
        );
    }
}
