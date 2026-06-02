//! Filesystem layout helpers for the migration tree.
//! Two responsibilities:
//! 1. **Path resolution.** Map a `(database, app)` [`BucketKey`] to
//!    the canonical on-disk paths for that bucket — committed
//!    migration files under `migrations/<database>/<app>/`, the
//!    snapshot at `migrations/<database>/<app>/schema_snapshot.json`,
//!    and the pending JSON staging file at
//!    `target/djogi_pending/<database>/<app>.json`.
//! 2. **Filesystem scanning.** For the build.rs three-way match and
//!    the D004 (folder drift) diagnostic, walk the on-disk
//!    `migrations/` tree and report which `(database, app)` pairs
//!    actually exist as directories. Compared against the snapshot's
//!    `registered_apps` to surface orphaned / missing folders.
//! # Workspace layout (frozen)
//! ```text
//! <workspace-root>/
//! ├── migrations/                              committed; git submodule
//! │   ├── main/
//! │   │   ├── billing/
//! │   │   │   ├── V20260425010203__add_invoices.sdjql
//! │   │   │   ├── V20260425010203__add_invoices.down.sdjql
//! │   │   │   └── schema_snapshot.json
//! │   │   └── _global_/                        synthetic bucket
//! │   │       └── …
//! │   └── crud_log/
//! │       └── audit/
//! │           └── …
//! └── target/                                  build artifact; gitignored
//!     ├── djogi_models.json                    written by `#[derive(Model)]`
//!     └── djogi_pending/                       written by `migrations compose`
//!         ├── main/
//!         │   └── billing.json
//!         └── crud_log/
//!             └── audit.json
//! ```
//! The synthetic global bucket (empty-string app label) lives at
//! `<database>/_global_/` on disk so file-system tooling does not have
//! to handle empty-string directory names. The path-resolution
//! helpers in this module map `BucketKey { app: "" }` to that
//! directory — the empty-string label remains the canonical in-memory
//! identity.
//! # No regex
//! Per the project-wide no-regex rule, the directory-listing scan
//! uses byte-level checks against `DirEntry::file_name`. The accepted
//! identifier grammar is the same as Postgres's: ASCII letter or
//! underscore, followed by ASCII alphanumerics or underscores, up to
//! 63 bytes — implemented byte-by-byte without any regex engine.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::naming::{MIGRATION_DOWN_SUFFIX, MIGRATION_FILE_EXT};
use super::projection::BucketKey;

/// Default committed-migrations directory name. Hard-coded so
/// every consumer agrees on the layout; future configurability lives
/// behind a `Djogi.toml::migrate.migrations_dir` field which falls
/// back to this constant.
pub const MIGRATIONS_DIR: &str = "migrations";

/// Default pending-staging directory name (relative to `target/`).
/// Matches the `target/djogi_pending/` path called out in the v3
/// plan §6 build.rs three-way match contract.
pub const PENDING_DIR: &str = "djogi_pending";

/// Filesystem token used for the synthetic global bucket (empty
/// in-memory label). Picked so the directory name is itself a valid
/// Postgres identifier — no leading underscore stripping, no
/// shell-quoting concerns. Tooling may scan for this verbatim token
/// when reconciling `migrations/<database>/<token>/` against the
/// in-memory `BucketKey { app: "" }`.
pub const GLOBAL_BUCKET_DIRNAME: &str = "_global_";

/// Filename of the per-bucket committed snapshot.
/// Mirrors the path called out in
/// [`crate::migrate::schema::AppliedSchema`] docs.
pub const SNAPSHOT_FILENAME: &str = "schema_snapshot.json";

/// Filename of the side-channel descriptor inventory written by
/// `#[derive(Model)]` (and read by build.rs). Lives at
/// `target/<this>` (the parent directory comes from `OUT_DIR` /
/// `CARGO_TARGET_DIR` resolution at build time, not from this
/// constant).
pub const MODELS_INVENTORY_FILENAME: &str = "djogi_models.json";

/// Convert a [`BucketKey`] app label to its on-disk directory name.
/// The synthetic global bucket (empty label) maps to
/// [`GLOBAL_BUCKET_DIRNAME`]; every other label is used verbatim.
/// Identifier validity is the responsibility of the projection layer
/// (which rejects malformed labels at registration time).
pub fn app_dirname(app_label: &str) -> &str {
    if app_label.is_empty() {
        GLOBAL_BUCKET_DIRNAME
    } else {
        app_label
    }
}

/// Inverse of [`app_dirname`]. Maps a directory name back to the
/// canonical in-memory app label. Used by the filesystem-scan path
/// (build.rs D004 diagnostic) to compare on-disk folders against the
/// snapshot's `registered_apps` list.
pub fn app_label_from_dirname(dirname: &str) -> &str {
    if dirname == GLOBAL_BUCKET_DIRNAME {
        ""
    } else {
        dirname
    }
}

/// Resolve the committed migrations directory under `workspace_root`.
pub fn migrations_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(MIGRATIONS_DIR)
}

/// Resolve the per-database directory under `migrations/`.
pub fn database_dir(workspace_root: &Path, database: &str) -> PathBuf {
    migrations_root(workspace_root).join(database)
}

/// Resolve the per-bucket directory under `migrations/<database>/`.
/// `bucket.app` is mapped through [`app_dirname`] so the synthetic
/// global bucket lands at `migrations/<database>/_global_/`.
pub fn bucket_dir(workspace_root: &Path, bucket: &BucketKey) -> PathBuf {
    database_dir(workspace_root, &bucket.database).join(app_dirname(&bucket.app))
}

/// Resolve the canonical snapshot path for a bucket
/// `migrations/<database>/<app>/schema_snapshot.json`.
pub fn snapshot_path(workspace_root: &Path, bucket: &BucketKey) -> PathBuf {
    bucket_dir(workspace_root, bucket).join(SNAPSHOT_FILENAME)
}

/// Resolve the pending-staging directory under `target/`.
pub fn pending_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target").join(PENDING_DIR)
}

/// Resolve the per-database pending directory under `target/djogi_pending/`.
pub fn pending_database_dir(workspace_root: &Path, database: &str) -> PathBuf {
    pending_root(workspace_root).join(database)
}

/// Resolve the per-bucket pending JSON path
/// `target/djogi_pending/<database>/<app>.json`. The app component
/// uses the same global-bucket mapping as the snapshot path.
pub fn pending_json_path(workspace_root: &Path, bucket: &BucketKey) -> PathBuf {
    pending_database_dir(workspace_root, &bucket.database)
        .join(super::naming::pending_json_filename(&bucket.app))
}

/// One `(database, app)` pair discovered on disk by [`scan_filesystem`].
/// Apps come back with the in-memory label form (empty string for
/// the global bucket); consumers comparing against the snapshot's
/// `registered_apps` list use the in-memory form on both sides.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FilesystemBucket {
    pub database: String,
    pub app: String,
}

/// Walk `migrations/` and return every `(database, app)` pair that
/// has a directory on disk.
/// **Read-only.** Never creates, deletes, or modifies any path.
/// Returns an empty set when `migrations/` does not exist (the typical
/// state of a fresh project before the first compose).
/// **Filtering.** Hidden directories (those whose name starts with
/// `b'.'`) are skipped. Files at any level are skipped. Non-UTF-8
/// directory names are skipped silently — they cannot match an
/// in-memory app label, which the projection layer enforces as ASCII.
/// **No regex.** The byte-level `is_acceptable_dir_name` filter is
/// the only sanity check applied to directory names; we leave full
/// identifier validation to the projection layer that owns the
/// canonical grammar.
pub fn scan_filesystem(workspace_root: &Path) -> Result<BTreeSet<FilesystemBucket>, io::Error> {
    scan_filesystem_filtered(workspace_root, None)
}

/// Like [`scan_filesystem`] but skips every database whose name does
/// not match `database_filter` *before* opening the per-database
/// directory. This is what consumers that only care about one
/// database (e.g. `db reset`) want — no point read_dir-ing dozens of
/// peer-database app directories just to discard them at the next
/// layer.
fn scan_filesystem_filtered(
    workspace_root: &Path,
    database_filter: Option<&str>,
) -> Result<BTreeSet<FilesystemBucket>, io::Error> {
    let mut out = BTreeSet::new();
    let migrations = migrations_root(workspace_root);
    let entries = match fs::read_dir(&migrations) {
        Ok(e) => e,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(database) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !is_acceptable_dir_name(database.as_bytes()) {
            continue;
        }
        if let Some(want) = database_filter
            && database != want
        {
            continue;
        }
        let database_path = entry.path();
        let app_entries = match fs::read_dir(&database_path) {
            Ok(e) => e,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        for app_entry in app_entries {
            let app_entry = app_entry?;
            if !app_entry.file_type()?.is_dir() {
                continue;
            }
            let Some(app_dir_name) = app_entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_acceptable_dir_name(app_dir_name.as_bytes()) {
                continue;
            }
            let label = app_label_from_dirname(&app_dir_name).to_string();
            out.insert(FilesystemBucket {
                database: database.clone(),
                app: label,
            });
        }
    }
    Ok(out)
}

/// Walk every up-side `.sdjql` file under `migrations/<database>/<app>/`
/// and return per-bucket `version → path` maps.
/// `database_filter`:
/// - `Some(name)` — only buckets whose `database` matches are included.
/// - `None` — every bucket discovered by [`scan_filesystem`] is included.
///   Filtering rules (shared by every consumer of this helper):
/// - Down-side files (suffix `.down.sdjql`) are skipped — the up-side
///   filename is the canonical version identifier.
/// - Files whose stem does not match the `V<14-digit>__<slug>` /
///   `V<14-digit>` grammar (per [`recover_version_from_stem`]) are
///   silently skipped.
/// - Buckets containing zero up-side migrations are absent from the
///   returned map (the per-bucket inner map is only created on first
///   insert).
///   **Legacy rejection.** Schema migration files with a `.sql` extension
///   (up or down side) are rejected with `io::ErrorKind::InvalidData` and
///   a diagnostic naming the file and version. Duplicate same-version
///   artifacts (e.g., `V1__x.sql` + `V1__x.sdjql`) produce a duplicate
///   diagnostic before single-file legacy rejection.
///   Returns `io::Error` directly so each caller can wrap it in a
///   crate-local error variant (`AttuneError::FilesystemScanFailed`,
///   `ResetError::MigrationScanFailed`, etc.).
pub fn scan_filesystem_with_files(
    workspace_root: &Path,
    database_filter: Option<&str>,
) -> Result<BTreeMap<BucketKey, BTreeMap<String, PathBuf>>, io::Error> {
    let mut out: BTreeMap<BucketKey, BTreeMap<String, PathBuf>> = BTreeMap::new();
    // Push the database filter into the first-level walk so we never
    // open peer-database app directories the caller doesn't care
    // about. The per-database short-circuit also keeps `db reset`
    // from triggering filesystem audits on unrelated databases.
    let buckets = scan_filesystem_filtered(workspace_root, database_filter)?;
    for fb in buckets {
        let bucket = BucketKey {
            database: fb.database,
            app: fb.app,
        };
        let dir = bucket_dir(workspace_root, &bucket);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        // Collect all V-prefixed candidates per bucket first, classifying
        // by recovered version. This allows us to detect duplicate same-version
        // artifacts (e.g., V1__x.sql + V1__x.sdjql) before reporting a
        // single-file legacy rejection.
        let mut version_candidates: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with('V') {
                continue;
            }
            // Skip down side for current extension.
            if name.ends_with(MIGRATION_DOWN_SUFFIX) {
                continue;
            }
            // Reject legacy down side explicitly with diagnostic.
            if name.ends_with(".down.sql") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("legacy schema migration down file rejected: {name}"),
                ));
            }
            // Classify extension: .sdjql (current) or .sql (legacy).
            let ext = if name.ends_with(MIGRATION_FILE_EXT) {
                MIGRATION_FILE_EXT
            } else if name.ends_with(".sql") {
                ".sql"
            } else {
                continue; // not a migration file
            };
            let stem = &name[..name.len() - ext.len()];
            let Some(version) = recover_version_from_stem(stem) else {
                continue;
            };
            version_candidates
                .entry(version)
                .or_default()
                .push(entry.path());
        }

        // Check for duplicates first (before legacy rejection), so that
        // V...__x.sql + V...__x.sdjql always produces the duplicate
        // diagnostic rather than a misleading single-file error.
        for (version, paths) in &version_candidates {
            if paths.len() > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate schema migration version {version}: {paths:?}"),
                ));
            }
        }

        // Insert into output, rejecting any remaining legacy .sql files.
        for (version, paths) in &version_candidates {
            let path = &paths[0]; // safe: we checked for duplicates above
            if path.extension().and_then(|e| e.to_str()) == Some("sql") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "legacy schema migration file rejected for {version}: {:?}; \
                         schema migrations must use .sdjql",
                        path
                    ),
                ));
            }
            out.entry(bucket.clone())
                .or_default()
                .insert(version.clone(), path.clone());
        }
    }
    Ok(out)
}

/// Recover the canonical version ID from a filename stem (the part
/// before the extension). The stem looks like `V20260425010203__add_users`;
/// the version is `V20260425010203__add_users` itself when the slug
/// is canonical, but for tests / edge cases we accept the bare prefix
/// `V20260425010203` too.
/// Implementation: walk a `V` followed by ASCII digits, then optional
/// `__<slug>`. The leading prefix produced by [`super::naming::version_prefix`]
/// is `V<14 ASCII digits>` so the deterministic case is straightforward.
pub(super) fn recover_version_from_stem(stem: &str) -> Option<String> {
    let bytes = stem.as_bytes();
    if bytes.is_empty() || bytes[0] != b'V' {
        return None;
    }
    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 {
        return None;
    }
    if i == bytes.len() {
        return Some(stem.to_string());
    }
    if i + 1 < bytes.len() && bytes[i] == b'_' && bytes[i + 1] == b'_' {
        return Some(stem.to_string());
    }
    None
}

/// Byte-level check: a directory name is acceptable iff it is
/// non-empty, the first byte is `b'_'` or
/// [`u8::is_ascii_alphabetic`], every subsequent byte is `b'_'` or
/// [`u8::is_ascii_alphanumeric`], and the total length is at most
/// 63 bytes (Postgres identifier limit).
/// No regex. The filter is intentionally conservative — anything
/// that fails this check is presumed to be an unrelated directory
/// (`.git`, `target`, hand-written README folder, etc.) and is
/// skipped silently. The projection layer enforces canonical
/// identifier grammar on registered app labels, so a real bucket
/// always satisfies this check.
fn is_acceptable_dir_name(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    // Reject dot-prefixed names (e.g. `.git`, `.github`) explicitly before the
    // general first-byte check so the rule is stated literally, not just as a
    // side-effect of the alphabetic / underscore gate below. The doc comment
    // mentions this rejection; the code now states it directly.
    if bytes[0] == b'.' {
        return false;
    }
    let first = bytes[0];
    if first != b'_' && !first.is_ascii_alphabetic() {
        return false;
    }
    for &b in &bytes[1..] {
        if b != b'_' && !b.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        // Per-test isolated temp root so parallel cargo-test runs do
        // not collide. Atomic counter makes the path unique even
        // when two tests in the same module construct one in the
        // same nanosecond.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("djogi-target-{tag}-{nanos}-{n}"))
    }

    #[test]
    fn app_dirname_maps_global_label() {
        assert_eq!(app_dirname(""), GLOBAL_BUCKET_DIRNAME);
        assert_eq!(app_dirname("billing"), "billing");
    }

    #[test]
    fn app_label_from_dirname_maps_global() {
        assert_eq!(app_label_from_dirname(GLOBAL_BUCKET_DIRNAME), "");
        assert_eq!(app_label_from_dirname("billing"), "billing");
    }

    #[test]
    fn snapshot_path_for_global_bucket() {
        let root = Path::new("/work");
        let bucket = BucketKey {
            database: "main".into(),
            app: "".into(),
        };
        assert_eq!(
            snapshot_path(root, &bucket),
            Path::new("/work/migrations/main/_global_/schema_snapshot.json")
        );
    }

    #[test]
    fn snapshot_path_for_named_app() {
        let root = Path::new("/work");
        let bucket = BucketKey {
            database: "crud_log".into(),
            app: "audit".into(),
        };
        assert_eq!(
            snapshot_path(root, &bucket),
            Path::new("/work/migrations/crud_log/audit/schema_snapshot.json")
        );
    }

    #[test]
    fn pending_json_path_for_global_bucket() {
        let root = Path::new("/work");
        let bucket = BucketKey {
            database: "main".into(),
            app: "".into(),
        };
        assert_eq!(
            pending_json_path(root, &bucket),
            Path::new("/work/target/djogi_pending/main/_global_.json")
        );
    }

    #[test]
    fn scan_filesystem_handles_missing_root() {
        let root = temp_root("missing");
        let buckets = scan_filesystem(&root).expect("ok");
        assert!(buckets.is_empty());
    }

    #[test]
    fn scan_filesystem_finds_two_buckets() {
        let root = temp_root("two");
        fs::create_dir_all(root.join("migrations/main/billing")).unwrap();
        fs::create_dir_all(root.join("migrations/main/_global_")).unwrap();
        fs::create_dir_all(root.join("migrations/crud_log/audit")).unwrap();
        let buckets = scan_filesystem(&root).expect("ok");
        let expect: BTreeSet<FilesystemBucket> = [
            FilesystemBucket {
                database: "crud_log".into(),
                app: "audit".into(),
            },
            FilesystemBucket {
                database: "main".into(),
                app: "".into(),
            },
            FilesystemBucket {
                database: "main".into(),
                app: "billing".into(),
            },
        ]
        .into_iter()
        .collect();
        assert_eq!(buckets, expect);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_filesystem_skips_files_and_hidden_dirs() {
        let root = temp_root("hidden");
        fs::create_dir_all(root.join("migrations/main/billing")).unwrap();
        fs::create_dir_all(root.join("migrations/.git/objects")).unwrap();
        fs::write(root.join("migrations/README.md"), "noop").unwrap();
        fs::write(root.join("migrations/main/billing/V1__init.sql"), "").unwrap();
        let buckets = scan_filesystem(&root).expect("ok");
        // `.git` starts with `.`; filter rejects.
        // `README.md` is a file at top level, not a directory; skipped.
        // The SQL file inside `billing/` does not become a bucket on
        // its own — only directories are reported.
        let expect: BTreeSet<FilesystemBucket> = [FilesystemBucket {
            database: "main".into(),
            app: "billing".into(),
        }]
        .into_iter()
        .collect();
        assert_eq!(buckets, expect);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn is_acceptable_dir_name_rules() {
        assert!(is_acceptable_dir_name(b"main"));
        assert!(is_acceptable_dir_name(b"_global_"));
        assert!(is_acceptable_dir_name(b"crud_log"));
        assert!(is_acceptable_dir_name(b"app1"));
        assert!(!is_acceptable_dir_name(b""));
        assert!(!is_acceptable_dir_name(b".git"));
        assert!(!is_acceptable_dir_name(b"1leading_digit"));
        assert!(!is_acceptable_dir_name(b"has-dash"));
        assert!(!is_acceptable_dir_name(b"has space"));
        // 63-byte boundary.
        let ok63: Vec<u8> = std::iter::repeat_n(b'a', 63).collect();
        assert!(is_acceptable_dir_name(&ok63));
        let bad64: Vec<u8> = std::iter::repeat_n(b'a', 64).collect();
        assert!(!is_acceptable_dir_name(&bad64));
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
    fn scan_filesystem_accepts_sdjql_extension() {
        let root = temp_root("sdjql-accept");
        let bucket = root.join("migrations/main/myapp");
        fs::create_dir_all(&bucket).unwrap();
        fs::write(bucket.join("V20260425010203__test.sdjql"), "SELECT 1;").unwrap();
        fs::write(bucket.join("V20260425010203__test.down.sdjql"), "SELECT 1;").unwrap();

        let result = scan_filesystem_with_files(&root, None).unwrap();
        let bk = BucketKey {
            database: "main".to_string(),
            app: "myapp".to_string(),
        };
        assert!(result.contains_key(&bk), "scanner must find .sdjql files");
        assert_eq!(result[&bk].len(), 1); // up only, down is skipped
    }

    #[test]
    fn scan_filesystem_rejects_legacy_sql_schema_migration_files() {
        let root = temp_root("legacy-sql-reject");
        let bucket = root.join("migrations/main/myapp");
        fs::create_dir_all(&bucket).unwrap();
        fs::write(bucket.join("V20260425010203__legacy.sql"), "SELECT 1;").unwrap();

        let result = scan_filesystem_with_files(&root, None);
        assert!(
            result.is_err(),
            "scanner must reject .sql schema migration files"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("legacy") || err.contains("V20260425010203__legacy.sql"),
            "error must mention legacy format or filename: {err}"
        );
    }

    #[test]
    fn scan_filesystem_detects_duplicate_same_version_artifacts() {
        let root = temp_root("duplicate-detect");
        let bucket = root.join("migrations/main/myapp");
        fs::create_dir_all(&bucket).unwrap();
        // Same version, different extensions — invalid state
        fs::write(bucket.join("V20260425010203__test.sql"), "SELECT 1;").unwrap();
        fs::write(bucket.join("V20260425010203__test.sdjql"), "SELECT 1;").unwrap();

        let result = scan_filesystem_with_files(&root, None);
        assert!(
            result.is_err(),
            "scanner must detect duplicate recovered-key artifacts"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("V20260425010203__test") && err.contains("duplicate"),
            "error must mention version and duplicates: {err}"
        );
    }

    #[test]
    fn scan_filesystem_skips_down_side_for_sdjql() {
        let root = temp_root("skip-down-sdjql");
        let bucket = root.join("migrations/main/myapp");
        fs::create_dir_all(&bucket).unwrap();
        fs::write(bucket.join("V20260425010203__a.sdjql"), "SELECT 1;").unwrap();
        fs::write(bucket.join("V20260425010203__a.down.sdjql"), "SELECT 1;").unwrap();
        fs::write(bucket.join("V20260425010204__b.sdjql"), "SELECT 1;").unwrap();
        fs::write(bucket.join("V20260425010204__b.down.sdjql"), "SELECT 1;").unwrap();

        let result = scan_filesystem_with_files(&root, None).unwrap();
        let bk = BucketKey {
            database: "main".to_string(),
            app: "myapp".to_string(),
        };
        assert_eq!(result[&bk].len(), 2); // two up files, no down files
    }
}
