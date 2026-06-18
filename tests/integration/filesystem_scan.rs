//!  — filesystem scan + D004 round-trip integration.
//!
//! Exercises the public `djogi::migrate::scan_filesystem` and
//! `djogi::migrate::classify_filesystem_drift` pipeline against a
//! real `migrations/<database>/<app>/` tree on disk. The
//! `build_match::tests` unit tests cover the pure-data path; this
//! test pins the tree-walking + diagnostic glue end-to-end.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use djogi::migrate::projection::BucketKey;
use djogi::migrate::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};
use djogi::migrate::{
    DriftKind, classify_filesystem_drift, create_workspace_dir_all, remove_workspace_dir_all,
    save_snapshot, scan_filesystem, write_workspace_file,
};

fn temp_root(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("djogi-t6-fs-{tag}-{nanos}-{n}"));
    create_workspace_dir_all(&std::env::temp_dir(), &p).unwrap();
    p
}

fn empty_snapshot(registered: Vec<String>) -> AppliedSchema {
    AppliedSchema {
        djogi_version: env!("CARGO_PKG_VERSION").to_string(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: "2026-04-25T00:00:00Z".to_string(),
        indexes: Vec::new(),
        models: BTreeMap::new(),
        registered_apps: registered,
    }
}

#[test]
fn scan_then_classify_no_drift_clean() {
    let root = temp_root("clean");
    create_workspace_dir_all(&root, root.join("migrations/main/_global_")).unwrap();
    let bucket = BucketKey {
        database: "main".into(),
        app: "".into(),
    };
    save_snapshot(
        &empty_snapshot(vec!["".to_string()]),
        &djogi::migrate::snapshot_path(&root, &bucket),
    )
    .unwrap();
    let fs_buckets = scan_filesystem(&root).unwrap();
    let mut snapshots = BTreeMap::new();
    snapshots.insert(bucket, empty_snapshot(vec!["".to_string()]));
    let diags = classify_filesystem_drift(&fs_buckets, &snapshots);
    assert!(diags.is_empty(), "no drift expected, got {diags:?}");
    let _ = remove_workspace_dir_all(&root, &root);
}

#[test]
fn scan_then_classify_unregistered_filesystem_app() {
    let root = temp_root("unregistered");
    // Two folders on disk, but the snapshot only registers one.
    create_workspace_dir_all(&root, root.join("migrations/main/_global_")).unwrap();
    create_workspace_dir_all(&root, root.join("migrations/main/ghost_app")).unwrap();
    let bucket = BucketKey {
        database: "main".into(),
        app: "".into(),
    };
    save_snapshot(
        &empty_snapshot(vec!["".to_string()]),
        &djogi::migrate::snapshot_path(&root, &bucket),
    )
    .unwrap();
    let fs_buckets = scan_filesystem(&root).unwrap();
    let mut snapshots = BTreeMap::new();
    snapshots.insert(bucket, empty_snapshot(vec!["".to_string()]));
    let diags = classify_filesystem_drift(&fs_buckets, &snapshots);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].kind, DriftKind::D004FilesystemUnregistered);
    assert!(diags[0].text.contains("ghost_app"));
    let _ = remove_workspace_dir_all(&root, &root);
}

#[test]
fn scan_then_classify_registered_missing_folder() {
    let root = temp_root("missing");
    // Snapshot lists two apps, only one folder exists on disk.
    create_workspace_dir_all(&root, root.join("migrations/main/_global_")).unwrap();
    let bucket = BucketKey {
        database: "main".into(),
        app: "".into(),
    };
    save_snapshot(
        &empty_snapshot(vec!["".to_string(), "billing".to_string()]),
        &djogi::migrate::snapshot_path(&root, &bucket),
    )
    .unwrap();
    let fs_buckets = scan_filesystem(&root).unwrap();
    let mut snapshots = BTreeMap::new();
    snapshots.insert(
        bucket,
        empty_snapshot(vec!["".to_string(), "billing".to_string()]),
    );
    let diags = classify_filesystem_drift(&fs_buckets, &snapshots);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].kind, DriftKind::D004RegisteredMissingFolder);
    assert!(diags[0].text.contains("billing"));
    let _ = remove_workspace_dir_all(&root, &root);
}

#[test]
fn scan_handles_multiple_databases() {
    let root = temp_root("multi_db");
    create_workspace_dir_all(&root, root.join("migrations/main/billing")).unwrap();
    create_workspace_dir_all(&root, root.join("migrations/crud_log/audit")).unwrap();
    let buckets = scan_filesystem(&root).unwrap();
    assert_eq!(buckets.len(), 2);
    let dbs: std::collections::BTreeSet<&str> =
        buckets.iter().map(|b| b.database.as_str()).collect();
    assert!(dbs.contains("main"));
    assert!(dbs.contains("crud_log"));
    let _ = remove_workspace_dir_all(&root, &root);
}

#[test]
fn scan_skips_dot_directories_and_files() {
    let root = temp_root("dot");
    create_workspace_dir_all(&root, root.join("migrations/main/billing")).unwrap();
    create_workspace_dir_all(&root, root.join("migrations/.git/refs")).unwrap();
    write_workspace_file(&root, root.join("migrations/README.md"), b"noop").unwrap();
    let buckets = scan_filesystem(&root).unwrap();
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets.iter().next().unwrap().app, "billing");
    let _ = remove_workspace_dir_all(&root, &root);
}
