//! Filesystem layout helpers for the Phase 7 migration tree.
//!
//! T6 owns this module. Two responsibilities:
//!
//! 1. **Path resolution.** Map a `(database, app)` [`BucketKey`] to
//!    the canonical on-disk paths for that bucket — committed
//!    migration files under `migrations/<database>/<app>/`, the
//!    snapshot at `migrations/<database>/<app>/schema_snapshot.json`,
//!    and the pending JSON staging file at
//!    `target/djogi_pending/<database>/<app>.json`.
//!
//! 2. **Filesystem scanning.** For the build.rs three-way match and
//!    the D004 (folder drift) diagnostic, walk the on-disk
//!    `migrations/` tree and report which `(database, app)` pairs
//!    actually exist as directories. Compared against the snapshot's
//!    `registered_apps` to surface orphaned / missing folders.
//!
//! # Workspace layout (frozen)
//!
//! ```text
//! <workspace-root>/
//! ├── migrations/                              committed; git submodule
//! │   ├── main/
//! │   │   ├── billing/
//! │   │   │   ├── V20260425010203__add_invoices.sql
//! │   │   │   ├── V20260425010203__add_invoices.down.sql
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
//!
//! The synthetic global bucket (empty-string app label) lives at
//! `<database>/_global_/` on disk so file-system tooling does not have
//! to handle empty-string directory names. The path-resolution
//! helpers in this module map `BucketKey { app: "" }` to that
//! directory — the empty-string label remains the canonical in-memory
//! identity.
//!
//! # No regex
//!
//! Per the project-wide no-regex rule, the directory-listing scan
//! uses byte-level checks against `DirEntry::file_name`. The accepted
//! identifier grammar is the same as Postgres's: ASCII letter or
//! underscore, followed by ASCII alphanumerics or underscores, up to
//! 63 bytes — implemented byte-by-byte without any regex engine.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::projection::BucketKey;

/// Default committed-migrations directory name. T6 hard-codes this so
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
///
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
///
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
///
/// `bucket.app` is mapped through [`app_dirname`] so the synthetic
/// global bucket lands at `migrations/<database>/_global_/`.
pub fn bucket_dir(workspace_root: &Path, bucket: &BucketKey) -> PathBuf {
    database_dir(workspace_root, &bucket.database).join(app_dirname(&bucket.app))
}

/// Resolve the canonical snapshot path for a bucket —
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

/// Resolve the per-bucket pending JSON path —
/// `target/djogi_pending/<database>/<app>.json`. The app component
/// uses the same global-bucket mapping as the snapshot path.
pub fn pending_json_path(workspace_root: &Path, bucket: &BucketKey) -> PathBuf {
    pending_database_dir(workspace_root, &bucket.database)
        .join(super::naming::pending_json_filename(&bucket.app))
}

/// One `(database, app)` pair discovered on disk by [`scan_filesystem`].
///
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
///
/// **Read-only.** Never creates, deletes, or modifies any path.
/// Returns an empty set when `migrations/` does not exist (the typical
/// state of a fresh project before the first compose).
///
/// **Filtering.** Hidden directories (those whose name starts with
/// `b'.'`) are skipped. Files at any level are skipped. Non-UTF-8
/// directory names are skipped silently — they cannot match an
/// in-memory app label, which the projection layer enforces as ASCII.
///
/// **No regex.** The byte-level `is_acceptable_dir_name` filter is
/// the only sanity check applied to directory names; we leave full
/// identifier validation to the projection layer that owns the
/// canonical grammar.
pub fn scan_filesystem(workspace_root: &Path) -> Result<BTreeSet<FilesystemBucket>, io::Error> {
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

/// Byte-level check: a directory name is acceptable iff it is
/// non-empty, the first byte is `b'_'` or
/// [`u8::is_ascii_alphabetic`], every subsequent byte is `b'_'` or
/// [`u8::is_ascii_alphanumeric`], and the total length is at most
/// 63 bytes (Postgres identifier limit).
///
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
}
